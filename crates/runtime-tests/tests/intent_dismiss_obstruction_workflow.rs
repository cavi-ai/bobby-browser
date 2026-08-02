use std::path::PathBuf;

use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, ServerConfig, StorageConfig};
use sdk_core::RuntimeService;
use types::{
    AttemptId, CommandEnvelope, CommandId, CommandOutcome, CreateSessionRequest,
    DismissObstructionIntent, ErrorCode, Evidence, ExecutionRecord, IntentCommand, IntentHints,
    IntentResolutionPath, NavigateCommand, OpenPageRequest, PageId, PrimitiveCommand,
    RuntimeCommand, SessionId, WaitUntil, WorkflowId,
};

fn intent_envelope(
    session_id: &SessionId,
    page_id: &PageId,
    command: IntentCommand,
) -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: session_id.clone(),
        page_id: Some(page_id.clone()),
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Intent(command),
    }
}

async fn completed_navigate(
    runtime: &RuntimeService,
    session_id: &SessionId,
    page_id: &PageId,
    url: String,
) {
    match runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session_id.clone(),
            page_id: Some(page_id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
                url,
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            })),
        })
        .await
    {
        CommandOutcome::Completed { .. } => {}
        outcome => panic!("navigate did not complete: {outcome:?}"),
    }
}

fn dismiss_intent(timeout_ms: u64) -> IntentCommand {
    IntentCommand::DismissObstruction(DismissObstructionIntent {
        purpose: "Close cookie notice".into(),
        hints: IntentHints {
            role: Some("button".into()),
            ..IntentHints::default()
        },
        timeout_ms,
    })
}

fn intent_record(evidence: &[Evidence]) -> ExecutionRecord {
    evidence
        .iter()
        .find_map(|item| match item {
            Evidence::IntentExecution { record } => Some(record.clone()),
            _ => None,
        })
        .expect("IntentExecution evidence")
}

async fn build_runtime(root: &std::path::Path) -> RuntimeService {
    let config = AppConfig {
        http: config::HttpConfig {
            allow_loopback: true,
            ..config::HttpConfig::default()
        },
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            shutdown_timeout_ms: 10_000,
        },
        browser: BrowserConfig {
            executable: Some(PathBuf::from(
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            )),
            profiles_dir: root.join("profiles"),
            headless: true,
            max_active: 1,
            upload_roots: Vec::new(),
            downloads_dir: root.join("downloads"),
            artifacts_dir: root.join("artifacts"),
            max_artifact_bytes: 8 * 1024 * 1024,
            max_screenshot_dimension: 16_384,
            max_js_result_bytes: 64 * 1024,
            max_js_timeout_ms: 30_000,
        },
        storage: StorageConfig {
            journal_path: root.join("commands.jsonl"),
            checkpoints_dir: root.join("checkpoints"),
            authority_path: root.join("authority.json"),
            scheduler_journal_path: root.join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
    };
    RuntimeService::build(&config).await.unwrap()
}

/// Live Chromium proof: DismissObstructionIntent clicks a described close
/// control and confirms — via built-in re-resolution, not a caller-supplied
/// expectation — that the obstruction actually left the DOM.
#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn dismiss_obstruction_removes_banner_on_live_chromium() {
    let fixture = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let runtime = build_runtime(root.path()).await;
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "intent-dismiss".into(),
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

    completed_navigate(
        &runtime,
        &session.id,
        &page.id,
        format!("{}/obstructed", fixture.base_url()),
    )
    .await;

    let outcome = runtime
        .submit(intent_envelope(
            &session.id,
            &page.id,
            dismiss_intent(5_000),
        ))
        .await;
    let CommandOutcome::Completed { evidence, .. } = outcome else {
        panic!("dismiss obstruction did not complete: {outcome:?}");
    };
    let record = intent_record(&evidence);
    assert_eq!(record.intent_kind, "dismissObstruction");
    assert_eq!(record.resolution_path, IntentResolutionPath::Deterministic);
    assert_eq!(record.verification, "dismissed");
    assert!(
        record.vision_proposal_sha256.is_none(),
        "deterministic path must not record a vision proposal"
    );
}

/// Live Chromium proof: when the click does not clear the obstruction, the
/// intent fails with `ObstructionSuspected` (surfaced here as
/// `VisionAssistDenied` since the session has not opted into vision), instead
/// of a false-positive completion.
#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn dismiss_obstruction_reports_stuck_when_banner_persists_on_live_chromium() {
    let fixture = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let runtime = build_runtime(root.path()).await;
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "intent-dismiss-stuck".into(),
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

    completed_navigate(
        &runtime,
        &session.id,
        &page.id,
        format!("{}/obstructed-stuck", fixture.base_url()),
    )
    .await;

    let outcome = runtime
        .submit(intent_envelope(&session.id, &page.id, dismiss_intent(300)))
        .await;
    let CommandOutcome::Failed { error, .. } = outcome else {
        panic!("dismiss obstruction should have failed on a persistent banner: {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::VisionAssistDenied);
    assert!(
        error.message.contains("obstructionPersisted"),
        "{}",
        error.message
    );
}
