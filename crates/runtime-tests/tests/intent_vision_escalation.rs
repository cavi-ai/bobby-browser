use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, ServerConfig, StorageConfig};
use intent_engine::{VisionAction, VisionAssist, VisionProposal, VisionProposeRequest};
use sdk_core::RuntimeService;
use types::{
    AttemptId, CommandEnvelope, CommandError, CommandId, CommandOutcome, CreateSessionRequest,
    Evidence, ExecutionPolicy, IntentCommand, IntentHints, IntentResolutionPath, LocateIntent,
    NavigateCommand, OpenPageRequest, PageId, PrimitiveCommand, RuntimeCommand, SessionId,
    WaitUntil, WorkflowId,
};

struct FakeVisionAssist {
    called: Arc<AtomicBool>,
}

#[async_trait]
impl VisionAssist for FakeVisionAssist {
    async fn propose(
        &self,
        _request: VisionProposeRequest,
    ) -> Result<VisionProposal, CommandError> {
        self.called.store(true, Ordering::SeqCst);
        Ok(VisionProposal {
            confidence: 0.95,
            // Harmless viewport click; proves RuntimeService → IntentEngine → click_xy.
            action: VisionAction::Click { x: 8.0, y: 8.0 },
        })
    }
}

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

/// Live Chromium proof that a stuck locate escalates through injected FakeVisionAssist
/// on RuntimeService and records `visionFallback`.
///
/// Unit coverage for vision gates/confidence lives in `intent-engine` tests; this harness
/// proves the AdaptivePageEngine injection path end-to-end.
#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn stuck_locate_uses_injected_fake_vision_assist() {
    let fixture = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let assist = Arc::new(FakeVisionAssist {
        called: called.clone(),
    });

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
            profiles_dir: root.path().join("profiles"),
            headless: true,
            max_active: 1,
            upload_roots: vec![root.path().to_path_buf()],
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
            scheduler_journal_path: root.path().join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
    };

    let runtime = RuntimeService::build_with_vision_assist(&config, assist)
        .await
        .unwrap();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "intent-vision".into(),
            proxy: None,
            execution_policy: ExecutionPolicy {
                javascript_evaluation: false,
                vision_assist: true,
            },
        })
        .await
        .unwrap();
    let page = runtime
        .open_page(OpenPageRequest {
            session_id: session.id.clone(),
        })
        .await
        .unwrap();

    match runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
                url: fixture.base_url(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            })),
        })
        .await
    {
        CommandOutcome::Completed { .. } => {}
        outcome => panic!("navigate failed: {outcome:?}"),
    }

    let outcome = runtime
        .submit_with_vision_capability(
            intent_envelope(
                &session.id,
                &page.id,
                IntentCommand::Locate(LocateIntent {
                    purpose: "Missing Action That Does Not Exist".into(),
                    hints: IntentHints {
                        role: Some("button".into()),
                        ..IntentHints::default()
                    },
                }),
            ),
            true,
        )
        .await;

    let CommandOutcome::Completed { evidence, .. } = outcome else {
        panic!("expected Completed visionFallback, got {outcome:?}");
    };
    assert!(
        called.load(Ordering::SeqCst),
        "FakeVisionAssist::propose must be invoked"
    );
    let record = evidence
        .iter()
        .find_map(|item| match item {
            Evidence::IntentExecution { record } => Some(record),
            _ => None,
        })
        .expect("IntentExecution evidence");
    assert_eq!(record.resolution_path, IntentResolutionPath::VisionFallback);
    assert_eq!(record.verification, "visionFallback");
    assert!(record.vision_proposal_sha256.is_some());
}
