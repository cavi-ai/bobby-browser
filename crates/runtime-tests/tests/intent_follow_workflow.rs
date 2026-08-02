use std::path::PathBuf;

use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, ServerConfig, StorageConfig};
use sdk_core::RuntimeService;
use types::{
    AttemptId, CheckpointId, CheckpointInvariant, CommandClass, CommandEnvelope, CommandId,
    CommandOutcome, CreateSessionRequest, ErrorCode, Evidence, ExecutionRecord, FollowIntent,
    IntentCommand, IntentHints, IntentResolutionPath, NavigateCommand, OpenPageRequest, PageId,
    PrimitiveCommand, RuntimeCommand, SessionId, TextMatch, WaitCondition, WaitForCommand,
    WaitUntil, WorkflowCheckpoint, WorkflowId,
};

fn primitive_envelope(
    session_id: &SessionId,
    page_id: &PageId,
    command: PrimitiveCommand,
) -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: session_id.clone(),
        page_id: Some(page_id.clone()),
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Primitive(command),
    }
}

fn intent_envelope(
    session_id: &SessionId,
    page_id: &PageId,
    workflow_id: WorkflowId,
    attempt_id: AttemptId,
    command_id: CommandId,
    command: IntentCommand,
) -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id,
        workflow_id,
        attempt_id,
        session_id: session_id.clone(),
        page_id: Some(page_id.clone()),
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Intent(command),
    }
}

async fn completed_primitive(
    runtime: &RuntimeService,
    session_id: &SessionId,
    page_id: &PageId,
    command: PrimitiveCommand,
) -> Vec<Evidence> {
    match runtime
        .submit(primitive_envelope(session_id, page_id, command))
        .await
    {
        CommandOutcome::Completed { evidence, .. } => evidence,
        outcome => panic!("primitive did not complete: {outcome:?}"),
    }
}

fn follow_intent(purpose: &str, boundary: bool) -> IntentCommand {
    IntentCommand::Follow(FollowIntent {
        purpose: purpose.into(),
        hints: IntentHints {
            role: Some("link".into()),
            ..IntentHints::default()
        },
        expected_destination: WaitForCommand {
            condition: WaitCondition::Url {
                matcher: TextMatch::Contains("/details".into()),
            },
            timeout_ms: 5_000,
        },
        boundary,
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

fn assert_deterministic_followed(record: &ExecutionRecord) {
    assert_eq!(record.intent_kind, "follow");
    assert_eq!(record.resolution_path, IntentResolutionPath::Deterministic);
    assert_eq!(record.verification, "followed");
    assert!(
        record.vision_proposal_sha256.is_none(),
        "deterministic path must not record a vision proposal"
    );
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
            executable: Some(PathBuf::from(&chrome_executable())),
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

/// Live Chromium proof: FollowIntent activates a same-tab link and verifies the
/// destination, for both the non-boundary (ordinary navigation) and boundary
/// (caller-flagged, checkpoint-gated) cases.
fn chrome_executable() -> std::path::PathBuf {
    std::env::var("BOBBY_CHROME_EXECUTABLE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        })
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn follow_intent_is_deterministic_on_live_chromium_for_both_boundary_states() {
    let fixture = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let runtime = build_runtime(root.path()).await;
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "intent-follow".into(),
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

    // --- boundary: false — plain navigation, no checkpoint required. ---
    completed_primitive(
        &runtime,
        &session.id,
        &page.id,
        PrimitiveCommand::Navigate(NavigateCommand {
            url: fixture.base_url(),
            wait_until: WaitUntil::Interactive,
            timeout_ms: 10_000,
        }),
    )
    .await;

    let follow_outcome = runtime
        .submit(intent_envelope(
            &session.id,
            &page.id,
            WorkflowId::new(),
            AttemptId::new(),
            CommandId::new(),
            follow_intent("Details", false),
        ))
        .await;
    let CommandOutcome::Completed { evidence, .. } = follow_outcome else {
        panic!("follow (boundary:false) did not complete: {follow_outcome:?}");
    };
    assert_deterministic_followed(&intent_record(&evidence));

    // --- boundary: true — rejected before a matching checkpoint exists, ... ---
    completed_primitive(
        &runtime,
        &session.id,
        &page.id,
        PrimitiveCommand::Navigate(NavigateCommand {
            url: fixture.base_url(),
            wait_until: WaitUntil::Interactive,
            timeout_ms: 10_000,
        }),
    )
    .await;

    let workflow_id = WorkflowId::new();
    let attempt_id = AttemptId::new();
    let boundary_follow_id = CommandId::new();
    let rejected = runtime
        .submit(intent_envelope(
            &session.id,
            &page.id,
            workflow_id.clone(),
            attempt_id.clone(),
            boundary_follow_id.clone(),
            follow_intent("Details", true),
        ))
        .await;
    let CommandOutcome::Failed { error, .. } = rejected else {
        panic!("boundary follow without checkpoint should be rejected: {rejected:?}");
    };
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("checkpoint"), "{}", error.message);

    // ... then succeeds through the Inspect -> Checkpoint -> Follow dance.
    let inspect_id = CommandId::new();
    let observed = match runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: inspect_id.clone(),
            workflow_id: workflow_id.clone(),
            attempt_id: attempt_id.clone(),
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(PrimitiveCommand::Inspect(
                types::InspectCommand::default(),
            )),
        })
        .await
    {
        CommandOutcome::Completed { evidence, .. } => evidence,
        outcome => panic!("pre-boundary inspection failed: {outcome:?}"),
    };
    let (current_url, title) = observed
        .iter()
        .find_map(|item| match item {
            Evidence::Inspection { url, title, .. } => Some((url.clone(), title.clone())),
            _ => None,
        })
        .expect("pre-boundary browser evidence");

    runtime
        .checkpoint(
            WorkflowCheckpoint {
                schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
                checkpoint_id: CheckpointId::new(),
                workflow_id: workflow_id.clone(),
                attempt_id: attempt_id.clone(),
                session_id: session.id.clone(),
                page_id: page.id.clone(),
                restart_url: fixture.base_url(),
                current_url: current_url.clone(),
                cursor: Some(inspect_id.clone()),
                boundary_command_id: Some(boundary_follow_id.clone()),
                recovery_class: CommandClass::Boundary,
                invariants: vec![
                    CheckpointInvariant::Url { value: current_url },
                    CheckpointInvariant::Title { value: title },
                ],
                replayable_inputs: Vec::new(),
                evidence: Vec::new(),
                recovery_history: Vec::new(),
                recovery_receipts: Vec::new(),
                created_at: Utc::now(),
            },
            vec![inspect_id],
        )
        .await
        .unwrap();

    let follow_outcome = runtime
        .submit(intent_envelope(
            &session.id,
            &page.id,
            workflow_id,
            attempt_id,
            boundary_follow_id,
            follow_intent("Details", true),
        ))
        .await;
    let CommandOutcome::Completed { evidence, .. } = follow_outcome else {
        panic!("follow (boundary:true) did not complete: {follow_outcome:?}");
    };
    assert_deterministic_followed(&intent_record(&evidence));
}
