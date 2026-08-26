//! Production wiring for ZigZagZig (godmode) sessions: a session created
//! with `zigzagzig: true` runs every page-bound command under the recovery
//! ladder, while a plain session's command fails once and returns.

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use chrono::{Duration, Utc};
use intent_engine::{VisionAssist, VisionProposal, VisionProposeRequest};
use sdk_core::RuntimeService;
use tokio::sync::Mutex;
use types::{
    AttemptId, ClickCommand, CommandEnvelope, CommandError, CommandId, CommandOutcome,
    CreateSessionRequest, Evidence, InspectCommand, NavigateCommand, OpenPageRequest, PageId,
    PrimitiveCommand, RuntimeCommand, SessionId, TypeTextCommand, WorkerId, WorkflowId,
};
use worker_pool::{BrowserWorker, WorkerFactory};

/// Stuck-click fixture: clicks land but never satisfy the postcondition, so
/// the command fails and the ladder (when active) climbs. The screenshot
/// flips the URL — the moment the vision solve loop runs, the blocked page
/// becomes reachable.
struct ChallengeFactory {
    clicks: Arc<AtomicUsize>,
    inspections: Arc<AtomicUsize>,
    current_url: Arc<Mutex<String>>,
}

struct ChallengeWorker {
    id: WorkerId,
    profile: PathBuf,
    clicks: Arc<AtomicUsize>,
    inspections: Arc<AtomicUsize>,
    current_url: Arc<Mutex<String>>,
}

#[async_trait]
impl WorkerFactory for ChallengeFactory {
    async fn launch(&self, _: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        Ok(Arc::new(ChallengeWorker {
            id: WorkerId::new(),
            profile: PathBuf::from("/profiles/zigzagzig-test"),
            clicks: Arc::clone(&self.clicks),
            inspections: Arc::clone(&self.inspections),
            current_url: Arc::clone(&self.current_url),
        }))
    }
}

#[async_trait]
impl BrowserWorker for ChallengeWorker {
    fn worker_id(&self) -> WorkerId {
        self.id.clone()
    }

    fn profile_dir(&self) -> &Path {
        &self.profile
    }

    async fn open_page(&self, _: PageId) -> Result<(), CommandError> {
        Ok(())
    }

    async fn navigate(
        &self,
        _: &PageId,
        command: &NavigateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        *self.current_url.lock().await = command.url.clone();
        Ok(vec![Evidence::Navigation {
            url: command.url.clone(),
            title: "ZigZagZig fixture".into(),
        }])
    }

    async fn inspect(
        &self,
        _: &PageId,
        command: &InspectCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.inspections.fetch_add(1, Ordering::SeqCst);
        Ok(vec![Evidence::Inspection {
            selector: command.selector.clone(),
            url: self.current_url.lock().await.clone(),
            title: "ZigZagZig fixture".into(),
            text: String::new(),
            html: None,
        }])
    }

    async fn click(
        &self,
        _: &PageId,
        command: &ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.clicks.fetch_add(1, Ordering::SeqCst);
        Ok(vec![Evidence::Element {
            selector: command.selector.clone(),
            text: None,
        }])
    }

    async fn type_text(
        &self,
        _: &PageId,
        _: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        unreachable!("the zigzagzig fixture only executes clicks")
    }

    async fn capture_screenshot(
        &self,
        _: &PageId,
        _: &types::CaptureScreenshotCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        *self.current_url.lock().await = "https://example.test/done".into();
        Ok(vec![Evidence::Screenshot {
            artifact_id: "shot-1".into(),
            media_type: "image/png".into(),
            width: 1,
            height: 1,
            bytes: 3,
            sha256: "abc".into(),
        }])
    }

    async fn screenshot_bytes(&self, _: &PageId) -> Result<Vec<u8>, CommandError> {
        Ok(b"png".to_vec())
    }

    async fn close(&self) -> Result<(), CommandError> {
        Ok(())
    }
}

/// Answers by intent kind: a challenge is present for detect, solved for
/// solve. The detection-first rung consults both.
struct SolveVision;

#[async_trait]
impl VisionAssist for SolveVision {
    async fn propose(&self, request: VisionProposeRequest) -> Result<VisionProposal, CommandError> {
        let action = if request.intent_kind == "detectChallenge" {
            intent_engine::VisionAction::ChallengeDetected {
                challenge_type: types::ChallengeType::RecaptchaV2Checkbox,
                region: None,
                blocking: true,
            }
        } else {
            intent_engine::VisionAction::ChallengeSolved
        };
        Ok(VisionProposal {
            confidence: 0.99,
            action,
        })
    }
}

fn fixture_config(root: &tempfile::TempDir) -> config::AppConfig {
    let mut config = config::AppConfig::default();
    config.storage.journal_path = root.path().join("commands.jsonl");
    config.storage.checkpoints_dir = root.path().join("checkpoints");
    config.browser.artifacts_dir = root.path().join("artifacts");
    config
}

fn stuck_click(session_id: SessionId, page_id: PageId) -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id,
        page_id: Some(page_id),
        deadline: Utc::now() + Duration::seconds(5),
        command: RuntimeCommand::Primitive(PrimitiveCommand::Click(ClickCommand {
            selector: "#blocked-target".into(),
            target: None,
            boundary: false,
            expected_url: Some("https://example.test/done".into()),
            modifiers: Vec::new(),
        })),
    }
}

struct Fixture {
    runtime: RuntimeService,
    clicks: Arc<AtomicUsize>,
    inspections: Arc<AtomicUsize>,
}

async fn build_fixture(vision: bool) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let clicks = Arc::new(AtomicUsize::new(0));
    let inspections = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(ChallengeFactory {
        clicks: Arc::clone(&clicks),
        inspections: Arc::clone(&inspections),
        current_url: Arc::new(Mutex::new("https://example.test/start".into())),
    });
    let config = fixture_config(&root);
    let runtime = if vision {
        RuntimeService::build_with_worker_factory_and_vision_assist(
            &config,
            factory,
            Arc::new(SolveVision),
        )
        .await
        .unwrap()
    } else {
        RuntimeService::build_with_worker_factory(&config, factory)
            .await
            .unwrap()
    };
    // The tempdir must outlive the runtime's journal/checkpoint handles.
    std::mem::forget(root);
    Fixture {
        runtime,
        clicks,
        inspections,
    }
}

async fn open_session_page(runtime: &RuntimeService, zigzagzig: bool) -> (SessionId, PageId) {
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "zigzagzig-test".into(),
            proxy: None,
            execution_policy: Default::default(),
            zigzagzig,
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

#[tokio::test]
async fn plain_session_fails_once_without_climbing_the_ladder() {
    let fixture = build_fixture(false).await;
    let (session_id, page_id) = open_session_page(&fixture.runtime, false).await;

    let outcome = fixture
        .runtime
        .submit(stuck_click(session_id, page_id))
        .await;

    assert!(matches!(outcome, CommandOutcome::RetryableFailure { .. }));
    assert_eq!(fixture.clicks.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn zigzagzig_session_climbs_the_ladder_on_a_stuck_command() {
    let fixture = build_fixture(false).await;
    let (session_id, page_id) = open_session_page(&fixture.runtime, true).await;

    let outcome = fixture
        .runtime
        .submit(stuck_click(session_id, page_id))
        .await;

    // The ladder climbed: the interaction-method rung retried the click and
    // the read-only rungs inspected the page. No checkpoint exists for the
    // workflow and no vision provider is attached, so the climb ends in
    // strategy exhaustion and the original failure stands.
    assert!(matches!(outcome, CommandOutcome::RetryableFailure { .. }));
    assert_eq!(fixture.clicks.load(Ordering::SeqCst), 2);
    assert!(fixture.inspections.load(Ordering::SeqCst) >= 2);
}

#[tokio::test]
async fn zigzagzig_session_solves_the_block_and_completes_the_command() {
    let fixture = build_fixture(true).await;
    let (session_id, page_id) = open_session_page(&fixture.runtime, true).await;

    let outcome = fixture
        .runtime
        .submit_with_vision_capability(stuck_click(session_id, page_id), true)
        .await;

    // The godmode path: observe -> re-resolve -> retry -> SolveChallenge.
    // The solve's screenshot cleared the block, the postcondition
    // re-observation saw /done, and the stuck click completed.
    let CommandOutcome::Completed { evidence, .. } = &outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    assert!(
        evidence.iter().any(|item| matches!(item, Evidence::Inspection { url, .. } if url == "https://example.test/done")),
        "postcondition evidence must show /done: {evidence:?}"
    );
    let tactics: Vec<&str> = evidence
        .iter()
        .filter_map(|item| match item {
            Evidence::Configuration { name, value } if name == "skillRecoveryTactic" => {
                Some(value.as_str())
            }
            _ => None,
        })
        .collect();
    assert!(
        tactics
            .iter()
            .any(|tactic| tactic.contains("solveChallenge")),
        "the solve rung must appear in tactic evidence: {tactics:?}"
    );
    assert_eq!(fixture.clicks.load(Ordering::SeqCst), 2);
}
