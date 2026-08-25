//! Live end-to-end proof of lazy batch vision prefill (Spec B T10):
//! prefill on resolves a multi-stuck-field form through the batch with
//! `VisionPrefill` evidence; prefill off resolves the same form through
//! per-field live escalation; provider loss never fails an intent the
//! deterministic path can finish.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, ServerConfig, StorageConfig};
use intent_engine::{VisionAction, VisionAssist, VisionProposal, VisionProposeRequest};
use sdk_core::RuntimeService;
use types::{
    AttemptId, CommandEnvelope, CommandError, CommandId, CommandOutcome, CompleteFormField,
    CompleteFormIntent, ControlAction, CreateSessionRequest, Evidence, ExecutionPolicy,
    IntentCommand, IntentResolutionPath, LocateIntent, NavigateCommand, OpenPageRequest, PageId,
    PrimitiveCommand, RuntimeCommand, SessionId, WaitUntil, WorkflowId,
};

struct CountingVision {
    propose_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl VisionAssist for CountingVision {
    async fn propose(
        &self,
        _request: VisionProposeRequest,
    ) -> Result<VisionProposal, CommandError> {
        self.propose_calls.fetch_add(1, Ordering::SeqCst);
        Ok(VisionProposal {
            confidence: 0.95,
            action: VisionAction::Click { x: 8.0, y: 8.0 },
        })
    }
}

struct OfflineVision;

#[async_trait]
impl VisionAssist for OfflineVision {
    async fn propose(
        &self,
        _request: VisionProposeRequest,
    ) -> Result<VisionProposal, CommandError> {
        Err(CommandError {
            code: types::ErrorCode::VisionAssistFailed,
            message: "connection refused".into(),
            layer: types::ErrorLayer::Page,
            retryable: false,
        })
    }
}

fn chrome_executable() -> PathBuf {
    std::env::var("BOBBY_CHROME_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        })
}

fn base_config(root: &std::path::Path, prefill: bool) -> AppConfig {
    AppConfig {
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
        storage: StorageConfig {
            journal_path: root.join("commands.jsonl"),
            checkpoints_dir: root.join("checkpoints"),
            authority_path: root.join("authority.json"),
            scheduler_journal_path: root.join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig {
            prefill,
            ..config::VisionConfig::default()
        },
        context: Default::default(),
        nodes: Default::default(),
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
    }
}

fn stuck_form() -> IntentCommand {
    IntentCommand::CompleteForm(CompleteFormIntent {
        purpose: "register".into(),
        fields: vec![
            CompleteFormField {
                name: "alpha".into(),
                purpose: "Missing Alpha Field That Does Not Exist".into(),
                hints: Default::default(),
                value: ControlAction::SetText {
                    value: "a".into(),
                    clear_first: true,
                },
            },
            CompleteFormField {
                name: "beta".into(),
                purpose: "Missing Beta Field That Does Not Exist".into(),
                hints: Default::default(),
                value: ControlAction::SetText {
                    value: "b".into(),
                    clear_first: true,
                },
            },
        ],
    })
}

async fn open_fixture(runtime: &RuntimeService, url: &str) -> (SessionId, PageId) {
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "vision-prefill".into(),
            proxy: None,
            execution_policy: ExecutionPolicy {
                javascript_evaluation: false,
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
                url: url.into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            })),
        })
        .await
    {
        CommandOutcome::Completed { .. } => {}
        outcome => panic!("navigate failed: {outcome:?}"),
    }
    (session.id, page.id)
}

async fn submit_intent(
    runtime: &RuntimeService,
    session_id: &SessionId,
    page_id: &PageId,
    command: IntentCommand,
) -> CommandOutcome {
    runtime
        .submit_with_vision_capability(
            CommandEnvelope {
                schema_version: CommandEnvelope::SCHEMA_VERSION,
                command_id: CommandId::new(),
                workflow_id: WorkflowId::new(),
                attempt_id: AttemptId::new(),
                session_id: session_id.clone(),
                page_id: Some(page_id.clone()),
                deadline: Utc::now() + Duration::seconds(30),
                command: RuntimeCommand::Intent(command),
            },
            true,
        )
        .await
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
async fn prefill_resolves_stuck_form_through_the_batch() {
    let fixture = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let propose_calls = Arc::new(AtomicUsize::new(0));
    let assist = Arc::new(CountingVision {
        propose_calls: propose_calls.clone(),
    });
    let config = base_config(root.path(), true);
    let runtime = RuntimeService::build_with_vision_assist(&config, assist)
        .await
        .unwrap();
    let (session, page) = open_fixture(&runtime, &fixture.base_url()).await;

    let outcome = submit_intent(&runtime, &session, &page, stuck_form()).await;
    let CommandOutcome::Completed { evidence, .. } = outcome else {
        panic!("expected Completed via prefill batch, got {outcome:?}");
    };
    assert_eq!(
        propose_calls.load(Ordering::SeqCst),
        2,
        "one propose per stuck purpose"
    );
    let paths = resolution_paths(&evidence);
    assert!(
        paths.contains(&IntentResolutionPath::VisionPrefill),
        "no VisionPrefill record in {paths:?}"
    );
    assert!(
        !paths.contains(&IntentResolutionPath::VisionFallback),
        "a live escalation ran despite the batch: {paths:?}"
    );
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn prefill_off_escalates_each_stuck_field_live() {
    let fixture = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let propose_calls = Arc::new(AtomicUsize::new(0));
    let assist = Arc::new(CountingVision {
        propose_calls: propose_calls.clone(),
    });
    let config = base_config(root.path(), false);
    let runtime = RuntimeService::build_with_vision_assist(&config, assist)
        .await
        .unwrap();
    let (session, page) = open_fixture(&runtime, &fixture.base_url()).await;

    let outcome = submit_intent(&runtime, &session, &page, stuck_form()).await;
    let CommandOutcome::Completed { evidence, .. } = outcome else {
        panic!("expected Completed via live escalation, got {outcome:?}");
    };
    assert_eq!(propose_calls.load(Ordering::SeqCst), 2);
    let paths = resolution_paths(&evidence);
    assert!(
        paths.contains(&IntentResolutionPath::VisionFallback),
        "no VisionFallback record in {paths:?}"
    );
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn provider_loss_never_fails_a_deterministically_resolvable_intent() {
    let fixture = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let config = base_config(root.path(), true);
    let runtime = RuntimeService::build_with_vision_assist(&config, Arc::new(OfflineVision))
        .await
        .unwrap();
    let (session, page) = open_fixture(&runtime, &fixture.base_url()).await;

    // The fixture's Continue button resolves deterministically: an offline
    // provider must be irrelevant to the outcome.
    let outcome = submit_intent(
        &runtime,
        &session,
        &page,
        IntentCommand::Locate(LocateIntent {
            purpose: "Continue".into(),
            hints: types::IntentHints {
                role: Some("button".into()),
                ..Default::default()
            },
        }),
    )
    .await;
    let CommandOutcome::Completed { .. } = outcome else {
        panic!("deterministic intent failed with an offline provider: {outcome:?}");
    };
}
