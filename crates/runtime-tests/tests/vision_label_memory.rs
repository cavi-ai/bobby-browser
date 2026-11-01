//! A purpose that vision resolved and the runtime verified is remembered on
//! the resolved control. A fresh runtime over the same context store fills
//! the same purpose from that memory without asking the vision node again.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use chrono::{Duration, Utc};
use config::{NodeConfig, NodeKind};
use sdk_core::RuntimeService;
use test_site::{FixtureSite, Route};
use types::{
    AttemptId, CommandEnvelope, CommandId, CommandOutcome, CompleteFormField, CompleteFormIntent,
    ControlAction, CreateSessionRequest, Evidence, ExecutionPolicy, FillIntent, IntentCommand,
    IntentHints, IntentResolutionPath, NavigateCommand, OpenPageRequest, PrimitiveCommand,
    RuntimeCommand, WaitUntil, WorkflowId,
};
use worker_pool::ChromiumWorkerFactory;

/// The visible caption is not the control's accessible name, so only vision
/// connects the purpose to the input.
const PAGE: &str = r#"<!doctype html><title>Account</title><main><form>
<div>Contact email</div><input id="f17" title="f17">
</form></main>"#;
const PURPOSE: &str = "Contact email";

fn chrome_executable() -> PathBuf {
    std::env::var("BOBBY_CHROME_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        })
}

/// A loopback vision node that types into the first offered candidate.
async fn typing_vision_node(served: Arc<AtomicUsize>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route(
            "/propose",
            axum::routing::post(move |_body: axum::body::Bytes| {
                let served = Arc::clone(&served);
                async move {
                    served.fetch_add(1, Ordering::SeqCst);
                    axum::Json(serde_json::json!({
                        "confidence": 0.9,
                        "action": {"kind": "typeIntoCandidate", "index": 0},
                    }))
                }
            }),
        )
        .layer(axum::extract::DefaultBodyLimit::disable());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}/propose")
}

fn config(root: &Path, endpoint: &str) -> config::AppConfig {
    config::AppConfig {
        http: config::HttpConfig {
            allow_loopback: true,
            ..config::HttpConfig::default()
        },
        server: config::ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            shutdown_timeout_ms: 10_000,
        },
        browser: config::BrowserConfig {
            executable: Some(chrome_executable()),
            profiles_dir: root.join("profiles"),
            headless: true,
            max_active: 1,
            upload_roots: vec![],
            downloads_dir: root.join("downloads"),
            artifacts_dir: root.join("artifacts"),
            max_artifact_bytes: 8 * 1024 * 1024,
            max_screenshot_dimension: 16_384,
            max_js_result_bytes: 64 * 1024,
            max_js_timeout_ms: 30_000,
        },
        storage: config::StorageConfig {
            journal_path: root.join("commands.jsonl"),
            checkpoints_dir: root.join("checkpoints"),
            authority_path: root.join("authority.json"),
            scheduler_journal_path: root.join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
        context: config::ContextConfig {
            dir: Some(root.join("context")),
            ..config::ContextConfig::default()
        },
        nodes: BTreeMap::from([(
            "vision".to_owned(),
            NodeConfig {
                kind: NodeKind::Vision,
                endpoint_url: endpoint.to_owned(),
                token_env: None,
                timeout_ms: 15_000,
            },
        )]),
    }
}

async fn runtime(config: &config::AppConfig) -> RuntimeService {
    let profile_id = config::EnginePreferenceConfig::ManagedChromium
        .durable_profile_id()
        .expect("managed Chromium carries a memory identity");
    RuntimeService::build_with_context_promotion(
        config,
        Arc::new(ChromiumWorkerFactory::new(config.browser.clone())),
        profile_id,
    )
    .await
    .unwrap()
}

fn envelope(
    session: &types::SessionId,
    page: &types::PageId,
    command: RuntimeCommand,
) -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: session.clone(),
        page_id: Some(page.clone()),
        deadline: Utc::now() + Duration::seconds(30),
        command,
    }
}

/// Opens the page in a new vision-enabled session and runs `intent`.
async fn visit(runtime: &RuntimeService, url: &str, intent: IntentCommand) -> Vec<Evidence> {
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "vision-label-memory".into(),
            proxy: None,
            execution_policy: ExecutionPolicy {
                vision_assist: true,
                ..ExecutionPolicy::default()
            },
            zigzagzig: false,
        })
        .await
        .unwrap();
    let page = runtime
        .open_page(OpenPageRequest {
            session_id: session.id.clone(),
        })
        .await
        .unwrap();
    let navigation = runtime
        .submit(envelope(
            &session.id,
            &page.id,
            RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
                url: url.into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            })),
        ))
        .await;
    assert!(
        matches!(navigation, CommandOutcome::Completed { .. }),
        "{navigation:?}"
    );
    let outcome = runtime
        .submit_with_vision_capability(
            envelope(&session.id, &page.id, RuntimeCommand::Intent(intent)),
            true,
        )
        .await;
    match outcome {
        CommandOutcome::Completed { evidence, .. } => evidence,
        outcome => panic!("intent failed: {outcome:?}"),
    }
}

fn email() -> ControlAction {
    ControlAction::SetText {
        value: "maya@example.test".into(),
        clear_first: true,
    }
}

fn fill() -> IntentCommand {
    IntentCommand::Fill(FillIntent {
        purpose: PURPOSE.into(),
        hints: IntentHints::default(),
        value: email(),
    })
}

fn complete_form() -> IntentCommand {
    IntentCommand::CompleteForm(CompleteFormIntent {
        purpose: "account".into(),
        fields: vec![CompleteFormField {
            name: "email".into(),
            purpose: PURPOSE.into(),
            hints: IntentHints::default(),
            value: email(),
            revealed_by: None,
        }],
    })
}

/// Every site file under the context store root, concatenated.
fn context_files(dir: &Path) -> String {
    let mut text = String::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            text.push_str(&context_files(&path));
        } else if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            text.push_str(&std::fs::read_to_string(&path).unwrap());
        }
    }
    text
}

fn resolution_paths(evidence: &[Evidence]) -> Vec<IntentResolutionPath> {
    evidence
        .iter()
        .filter_map(|item| match item {
            Evidence::IntentExecution { record } => Some(record.resolution_path),
            _ => None,
        })
        .collect()
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn a_vision_verified_label_resolves_the_next_visit_without_vision() {
    let site = FixtureSite::spawn(vec![("/account", Route::Html(PAGE.into()))]).await;
    let url = site.url("/account");
    let served = Arc::new(AtomicUsize::new(0));
    let endpoint = typing_vision_node(Arc::clone(&served)).await;
    let root = tempfile::tempdir().unwrap();
    let config = config(root.path(), &endpoint);

    let first = runtime(&config).await;
    let evidence = visit(&first, &url, fill()).await;
    assert_eq!(served.load(Ordering::SeqCst), 1, "visit 1 must ask vision");
    assert_eq!(
        resolution_paths(&evidence),
        vec![IntentResolutionPath::VisionFallback]
    );
    drop(first);
    let stored = context_files(&root.path().join("context"));
    assert!(stored.contains("\"contact email\""), "{stored}");
    assert!(
        !stored.contains("maya@example.test"),
        "a typed value was stored"
    );

    let second = runtime(&config).await;
    let evidence = visit(&second, &url, fill()).await;
    assert_eq!(
        served.load(Ordering::SeqCst),
        1,
        "visit 2 asked vision for a purpose the store already verified"
    );
    assert_eq!(
        resolution_paths(&evidence),
        vec![IntentResolutionPath::Context]
    );
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn a_remembered_form_field_needs_no_prefill_request() {
    let site = FixtureSite::spawn(vec![("/account", Route::Html(PAGE.into()))]).await;
    let url = site.url("/account");
    let served = Arc::new(AtomicUsize::new(0));
    let endpoint = typing_vision_node(Arc::clone(&served)).await;
    let root = tempfile::tempdir().unwrap();
    let config = config(root.path(), &endpoint);

    let first = runtime(&config).await;
    let evidence = visit(&first, &url, complete_form()).await;
    assert_eq!(served.load(Ordering::SeqCst), 1, "visit 1 must ask vision");
    assert!(
        !resolution_paths(&evidence).contains(&IntentResolutionPath::Context),
        "{:?}",
        resolution_paths(&evidence)
    );
    drop(first);

    let second = runtime(&config).await;
    let evidence = visit(&second, &url, complete_form()).await;
    assert_eq!(
        served.load(Ordering::SeqCst),
        1,
        "visit 2 asked vision for a field the store already verified"
    );
    assert_eq!(
        resolution_paths(&evidence),
        vec![IntentResolutionPath::Context]
    );
}
