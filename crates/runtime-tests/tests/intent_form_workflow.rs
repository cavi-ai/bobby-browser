use std::path::PathBuf;

use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, ServerConfig, StorageConfig};
use sdk_core::RuntimeService;
use types::{
    AttemptId, CheckpointId, CheckpointInvariant, ClickCommand, CommandClass, CommandEnvelope,
    CommandId, CommandOutcome, CreateSessionRequest, ElementState, Evidence, ExecutionRecord,
    FillIntent, FillValue, InspectCommand, IntentCommand, IntentHints, IntentResolutionPath,
    LocateIntent, NavigateCommand, OpenPageRequest, PageId, PrimitiveCommand, RuntimeCommand,
    SessionId, SubmitAndVerifyIntent, TargetSpec, TextMatch, WaitCondition, WaitForCommand,
    WaitForStateIntent, WaitUntil, WorkflowCheckpoint, WorkflowId,
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

async fn completed_intent(
    runtime: &RuntimeService,
    session_id: &SessionId,
    page_id: &PageId,
    command: IntentCommand,
) -> ExecutionRecord {
    match runtime
        .submit(intent_envelope(
            session_id,
            page_id,
            WorkflowId::new(),
            AttemptId::new(),
            CommandId::new(),
            command,
        ))
        .await
    {
        CommandOutcome::Completed { evidence, .. } => intent_record(&evidence),
        outcome => panic!("intent did not complete: {outcome:?}"),
    }
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

fn assert_deterministic(record: &ExecutionRecord, kind: &str) {
    assert_eq!(record.intent_kind, kind);
    assert_eq!(record.resolution_path, IntentResolutionPath::Deterministic);
    assert!(
        record.vision_proposal_sha256.is_none(),
        "deterministic path must not record a vision proposal"
    );
}

/// Live Chromium multi-step intent proof against the vertical-slice fixture:
/// locate → fill (text + file) → wait-for-state → submit-and-verify.
fn chrome_executable() -> std::path::PathBuf {
    std::env::var("BOBBY_CHROME_EXECUTABLE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        })
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn intent_form_workflow_is_deterministic_on_live_chromium() {
    let fixture = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let uploads_dir = root.path().join("uploads");
    std::fs::create_dir(&uploads_dir).unwrap();
    let resume = uploads_dir.join("resume.txt");
    std::fs::write(&resume, b"Ada Lovelace").unwrap();

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
            profiles_dir: root.path().join("profiles"),
            headless: true,
            max_active: 1,
            upload_roots: vec![uploads_dir],
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
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
    };

    let runtime = RuntimeService::build(&config).await.unwrap();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "intent-form".into(),
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

    let locate = completed_intent(
        &runtime,
        &session.id,
        &page.id,
        IntentCommand::Locate(LocateIntent {
            purpose: "Continue".into(),
            hints: IntentHints {
                role: Some("button".into()),
                ..IntentHints::default()
            },
        }),
    )
    .await;
    assert_deterministic(&locate, "locate");
    assert_eq!(locate.verification, "resolved");

    let fill_name = completed_intent(
        &runtime,
        &session.id,
        &page.id,
        IntentCommand::Fill(FillIntent {
            purpose: "Name".into(),
            hints: IntentHints {
                role: Some("textbox".into()),
                ..IntentHints::default()
            },
            value: FillValue::Text {
                text: "Ada".into(),
                clear_first: true,
            },
        }),
    )
    .await;
    assert_deterministic(&fill_name, "fill");

    let fill_resume = completed_intent(
        &runtime,
        &session.id,
        &page.id,
        IntentCommand::Fill(FillIntent {
            purpose: "Resume".into(),
            hints: IntentHints {
                role: Some("button".into()),
                ..IntentHints::default()
            },
            value: FillValue::Files {
                paths: vec![resume.to_string_lossy().into_owned()],
            },
        }),
    )
    .await;
    assert_deterministic(&fill_resume, "fill");

    // Locate resolves without acting; advance the fixture step with a primitive click.
    completed_primitive(
        &runtime,
        &session.id,
        &page.id,
        PrimitiveCommand::Click(ClickCommand {
            selector: String::new(),
            target: Some(TargetSpec {
                role: Some("button".into()),
                accessible_name: Some("Continue".into()),
                ..TargetSpec::default()
            }),
            boundary: false,
            expected_url: None,
        }),
    )
    .await;

    let wait = completed_intent(
        &runtime,
        &session.id,
        &page.id,
        IntentCommand::WaitForState(WaitForStateIntent {
            condition: WaitCondition::Element {
                target: Box::new(TargetSpec {
                    css: Some("#company".into()),
                    ..TargetSpec::default()
                }),
                state: ElementState::Visible,
            },
            timeout_ms: 5_000,
        }),
    )
    .await;
    assert_deterministic(&wait, "waitForState");

    let fill_company = completed_intent(
        &runtime,
        &session.id,
        &page.id,
        IntentCommand::Fill(FillIntent {
            purpose: "Company".into(),
            hints: IntentHints {
                role: Some("textbox".into()),
                ..IntentHints::default()
            },
            value: FillValue::Text {
                text: "Analytical Engines".into(),
                clear_first: true,
            },
        }),
    )
    .await;
    assert_deterministic(&fill_company, "fill");

    let workflow_id = WorkflowId::new();
    let attempt_id = AttemptId::new();
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
            command: RuntimeCommand::Primitive(
                PrimitiveCommand::Inspect(InspectCommand::default()),
            ),
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
    let submit_id = CommandId::new();
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
                boundary_command_id: Some(submit_id.clone()),
                recovery_class: CommandClass::Boundary,
                invariants: vec![
                    CheckpointInvariant::Url { value: current_url },
                    CheckpointInvariant::Title { value: title },
                ],
                replayable_inputs: vec!["Ada".into(), "Analytical Engines".into()],
                evidence: Vec::new(),
                recovery_history: Vec::new(),
                recovery_receipts: Vec::new(),
                created_at: Utc::now(),
            },
            vec![inspect_id],
        )
        .await
        .unwrap();

    let submit_outcome = runtime
        .submit(intent_envelope(
            &session.id,
            &page.id,
            workflow_id,
            attempt_id,
            submit_id,
            IntentCommand::SubmitAndVerify(SubmitAndVerifyIntent {
                purpose: "Submit".into(),
                hints: IntentHints {
                    role: Some("button".into()),
                    ..IntentHints::default()
                },
                expected_state: WaitForCommand {
                    condition: WaitCondition::Url {
                        matcher: TextMatch::Contains("/complete".into()),
                    },
                    timeout_ms: 5_000,
                },
            }),
        ))
        .await;
    let CommandOutcome::Completed { evidence, .. } = submit_outcome else {
        panic!("submitAndVerify did not complete: {submit_outcome:?}");
    };
    let submit = intent_record(&evidence);
    assert_deterministic(&submit, "submitAndVerify");
    assert_eq!(submit.verification, "submitted");
}
