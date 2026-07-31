use std::collections::BTreeMap;

use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, HttpConfig, ServerConfig, StorageConfig};
use network_engine::state::HttpStateSnapshot;
use network_engine::{DestinationPolicy, DirectHttpExecutor, NetworkPolicy};
use sdk_core::RuntimeService;
use types::{
    AttemptId, CommandEnvelope, CommandId, CommandOutcome, CreateSessionRequest,
    DownloadUrlCommand, ErrorCode, Evidence, InspectCommand, NavigateCommand, OpenPageRequest,
    PageId, PrimitiveCommand, RuntimeCommand, SessionId, WaitUntil, WorkflowId,
};

fn snapshot(url: String) -> HttpStateSnapshot {
    HttpStateSnapshot {
        version: 1,
        current_url: url,
        cookies: Vec::new(),
        cache_validators: BTreeMap::new(),
        user_agent: "security-proof".into(),
        language: "en-US".into(),
    }
}

fn config(root: &tempfile::TempDir, max_download_bytes: usize) -> AppConfig {
    AppConfig {
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            shutdown_timeout_ms: 10_000,
        },
        browser: BrowserConfig {
            executable: None,
            profiles_dir: root.path().join("profiles"),
            headless: true,
            max_active: 2,
            upload_roots: vec![root.path().join("uploads")],
            downloads_dir: root.path().join("downloads"),
            artifacts_dir: root.path().join("artifacts"),
            max_artifact_bytes: 8 * 1024 * 1024,
            max_screenshot_dimension: 16_384,
            max_js_result_bytes: 64 * 1024,
            max_js_timeout_ms: 30_000,
        },
        storage: StorageConfig {
            journal_path: root.path().join("commands.jsonl"),
            checkpoints_dir: root.path().join("checkpoints"),
            authority_path: root.path().join("authority.json"),
        },
        http: HttpConfig {
            allow_loopback: true,
            max_body_bytes: 64,
            max_download_bytes,
            ..HttpConfig::default()
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
    }
}

fn envelope(session: &SessionId, page: &PageId, command: PrimitiveCommand) -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: session.clone(),
        page_id: Some(page.clone()),
        deadline: Utc::now() + Duration::seconds(15),
        command: RuntimeCommand::Primitive(command),
    }
}

async fn completed(
    runtime: &RuntimeService,
    session: &SessionId,
    page: &PageId,
    command: PrimitiveCommand,
) -> Vec<Evidence> {
    match runtime.submit(envelope(session, page, command)).await {
        CommandOutcome::Completed { evidence, .. } => evidence,
        outcome => panic!("expected completed command, got {outcome:?}"),
    }
}

async fn session_page(runtime: &RuntimeService, profile: &str) -> (SessionId, PageId) {
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: profile.into(),
            proxy: None,
            execution_policy: Default::default(),
        })
        .await
        .unwrap();
    let page = runtime
        .open_page(OpenPageRequest {
            session_id: session.id.clone(),
        })
        .await
        .unwrap();
    (session.id, page.id)
}

fn text(evidence: &[Evidence]) -> &str {
    evidence
        .iter()
        .find_map(|item| match item {
            Evidence::Inspection { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .expect("inspection evidence")
}

fn assert_empty_session_artifacts(root: &tempfile::TempDir, session: &SessionId) {
    let directory = root.path().join("artifacts").join(session.0.to_string());
    assert!(
        !directory.exists() || std::fs::read_dir(directory).unwrap().next().is_none(),
        "failed download left committed or staging artifact entries"
    );
}

#[tokio::test]
async fn production_denial_and_explicit_fixture_grant_are_enforced_per_hop() {
    let site = test_site::spawn().await;
    let production = DirectHttpExecutor::new(NetworkPolicy::default())
        .inspect(
            &snapshot(format!("{}/static", site.base_url())),
            &InspectCommand::default(),
        )
        .await;
    assert!(matches!(production, Err(error) if error.code == ErrorCode::NetworkPolicyDenied));

    let redirect = DirectHttpExecutor::new(NetworkPolicy {
        allow_loopback: true,
        ..NetworkPolicy::default()
    })
    .inspect(
        &snapshot(format!("{}/redirect-private", site.base_url())),
        &InspectCommand::default(),
    )
    .await;
    assert!(matches!(redirect, Err(error) if error.code == ErrorCode::NetworkPolicyDenied));
}

#[test]
fn production_destination_policy_rejects_metadata_link_local_and_dns_rebinding_families() {
    let policy = DestinationPolicy::new(NetworkPolicy::default());
    let url = reqwest::Url::parse("https://public.example/").unwrap();

    for denied in [
        "169.254.169.254:443",
        "[fe80::1]:443",
        "[fd00:ec2::254]:443",
    ] {
        let result = policy.validate_resolved(url.clone(), vec![denied.parse().unwrap()]);
        assert!(
            matches!(result, Err(error) if error.code == ErrorCode::NetworkPolicyDenied),
            "metadata or link-local destination {denied} crossed the production policy boundary"
        );
    }

    let initially_public = policy
        .validate_resolved(url.clone(), vec!["93.184.216.34:443".parse().unwrap()])
        .unwrap();
    assert_eq!(initially_public.addresses().len(), 1);
    let rebound = policy.validate_resolved(
        url,
        vec![
            "93.184.216.34:443".parse().unwrap(),
            "169.254.169.254:443".parse().unwrap(),
        ],
    );
    assert!(
        matches!(rebound, Err(error) if error.code == ErrorCode::NetworkPolicyDenied),
        "a later mixed DNS answer must be reevaluated and denied"
    );
    println!("AUTOMATION_RUNTIME_SECURITY_PROOF:v1:adaptive-http-policy");
}

#[tokio::test]
async fn real_runtime_sessions_isolate_direct_http_cookie_state() {
    let site = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let runtime = RuntimeService::build(&config(&root, 64)).await.unwrap();
    let (owner, owner_page) = session_page(&runtime, "cookie-owner").await;
    let (stranger, stranger_page) = session_page(&runtime, "cookie-stranger").await;
    let echo = format!("{}/cookie-echo", site.base_url());
    for (session, page) in [(&owner, &owner_page), (&stranger, &stranger_page)] {
        completed(
            &runtime,
            session,
            page,
            PrimitiveCommand::Navigate(NavigateCommand {
                url: echo.clone(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            }),
        )
        .await;
    }

    completed(
        &runtime,
        &owner,
        &owner_page,
        PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
            url: format!("{}/download", site.base_url()),
            expected_content_type: Some("application/octet-stream".into()),
            max_bytes: 20,
        }),
    )
    .await;
    let journal = tokio::fs::read_to_string(root.path().join("commands.jsonl"))
        .await
        .unwrap();
    assert!(
        !journal.contains("\"value\":\"yes\"") && !journal.contains("\"downloaded\":\"yes\""),
        "prepared HTTP state leaked a cookie value"
    );
    let prepared_records: Vec<serde_json::Value> = journal
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|record: &serde_json::Value| record["phase"] == "resultPrepared")
        .collect();
    assert!(!prepared_records.is_empty());
    assert!(prepared_records
        .iter()
        .all(|record| record["preparedResult"]["stateDelta"].is_null()));
    let owner_echo = completed(
        &runtime,
        &owner,
        &owner_page,
        PrimitiveCommand::Inspect(InspectCommand::default()),
    )
    .await;
    let stranger_echo = completed(
        &runtime,
        &stranger,
        &stranger_page,
        PrimitiveCommand::Inspect(InspectCommand::default()),
    )
    .await;
    assert!(text(&owner_echo).contains("downloaded=yes"));
    assert!(!text(&stranger_echo).contains("downloaded=yes"));
}

#[tokio::test]
async fn runtime_download_failures_clean_real_artifact_store_and_redact_secret_journal_state() {
    let site = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let runtime = RuntimeService::build(&config(&root, 64)).await.unwrap();
    let (session, page) = session_page(&runtime, "failure-owner").await;

    for (route, max_bytes) in [("interrupted", 64), ("download", 19)] {
        let outcome = runtime
            .submit(envelope(
                &session,
                &page,
                PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
                    url: format!("{}/{route}", site.base_url()),
                    expected_content_type: Some("application/octet-stream".into()),
                    max_bytes,
                }),
            ))
            .await;
        assert!(
            matches!(
                outcome,
                CommandOutcome::Failed { .. }
                    | CommandOutcome::RetryableFailure { .. }
                    | CommandOutcome::NeedsReconciliation { .. }
            ),
            "{route} returned unexpected outcome: {outcome:?}"
        );
        assert!(!format!("{outcome:?}").contains("Download {"));
        assert_empty_session_artifacts(&root, &session);
    }

    let secret = "super-secret-capability";
    let outcome = runtime
        .submit(envelope(
            &session,
            &page,
            PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
                url: format!("{}/download-secret-cookie", site.base_url()),
                expected_content_type: Some("application/octet-stream".into()),
                max_bytes: 64,
            }),
        ))
        .await;
    assert!(matches!(
        outcome,
        CommandOutcome::Failed { .. } | CommandOutcome::NeedsReconciliation { .. }
    ));
    let serialized_outcome = serde_json::to_string(&outcome).unwrap();
    assert!(!serialized_outcome.contains(secret));
    assert!(!serialized_outcome.contains("Download"));
    assert_empty_session_artifacts(&root, &session);

    let journal = tokio::fs::read_to_string(root.path().join("commands.jsonl"))
        .await
        .unwrap();
    assert!(!journal.contains(secret));
    assert!(!journal.contains("download-secret-cookie") || !journal.contains(secret));
}
