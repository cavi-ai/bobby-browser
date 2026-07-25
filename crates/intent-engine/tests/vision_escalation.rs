use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use intent_engine::{
    IntentBrowser, IntentEngine, IntentOutcome, VisionAction, VisionAssist, VisionContext,
    VisionProposal, VisionProposeRequest, VISION_CONFIDENCE_FLOOR,
};
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandError, ErrorCode, Evidence, IntentCommand,
    IntentHints, IntentResolutionPath, LocateIntent, PageId, TargetSpec, TypeTextCommand,
    UploadFilesCommand, WaitForCommand,
};

struct FakeVision {
    called: Arc<AtomicBool>,
    proposal: VisionProposal,
}

#[async_trait]
impl VisionAssist for FakeVision {
    async fn propose(
        &self,
        _request: VisionProposeRequest,
    ) -> Result<VisionProposal, CommandError> {
        self.called.store(true, Ordering::SeqCst);
        Ok(self.proposal.clone())
    }
}

#[derive(Default)]
struct FakeBrowser {
    candidates: Vec<dom_engine::Candidate>,
    gather_error: Option<CommandError>,
    click_xy_calls: Arc<AtomicUsize>,
    screenshot_png: Vec<u8>,
}

#[async_trait]
impl IntentBrowser for FakeBrowser {
    async fn collect_candidates(
        &self,
        _page_id: &PageId,
        _target: &TargetSpec,
    ) -> Result<Vec<dom_engine::Candidate>, CommandError> {
        if let Some(error) = &self.gather_error {
            return Err(error.clone());
        }
        Ok(self.candidates.clone())
    }

    async fn click(
        &self,
        _page_id: &PageId,
        _command: &ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported("click"))
    }

    async fn click_xy(
        &self,
        _page_id: &PageId,
        _x: f64,
        _y: f64,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.click_xy_calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![Evidence::Configuration {
            name: "visionClick".into(),
            value: "ok".into(),
        }])
    }

    async fn type_text(
        &self,
        _page_id: &PageId,
        _command: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported("type_text"))
    }

    async fn upload_files(
        &self,
        _page_id: &PageId,
        _command: &UploadFilesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported("upload_files"))
    }

    async fn wait_for(
        &self,
        _page_id: &PageId,
        _command: &WaitForCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported("wait_for"))
    }

    async fn capture_screenshot(
        &self,
        _page_id: &PageId,
        _command: &CaptureScreenshotCommand,
    ) -> Result<(Vec<u8>, Vec<Evidence>), CommandError> {
        Ok((
            self.screenshot_png.clone(),
            vec![Evidence::Screenshot {
                artifact_id: "shot-1".into(),
                media_type: "image/png".into(),
                width: 1,
                height: 1,
                bytes: self.screenshot_png.len() as u64,
                sha256: "abc".into(),
            }],
        ))
    }
}

fn unsupported(op: &str) -> CommandError {
    CommandError {
        code: ErrorCode::Internal,
        message: format!("{op} not supported by fake browser"),
        layer: types::ErrorLayer::Page,
        retryable: false,
    }
}

fn locate() -> IntentCommand {
    IntentCommand::Locate(LocateIntent {
        purpose: "Continue".into(),
        hints: IntentHints {
            role: Some("button".into()),
            ..IntentHints::default()
        },
    })
}

fn click_proposal(confidence: f32) -> VisionProposal {
    VisionProposal {
        confidence,
        action: VisionAction::Click { x: 12.0, y: 34.0 },
    }
}

#[tokio::test]
async fn stuck_without_vision_gates_returns_vision_assist_denied() {
    let called = Arc::new(AtomicBool::new(false));
    let assist = Arc::new(FakeVision {
        called: called.clone(),
        proposal: click_proposal(0.99),
    });
    let browser = FakeBrowser {
        screenshot_png: b"png".to_vec(),
        ..FakeBrowser::default()
    };
    let page_id = PageId::new();

    let outcome = IntentEngine::execute(
        &locate(),
        &page_id,
        &browser,
        &VisionContext {
            session_ok: false,
            capability_ok: false,
            assist: Some(assist),
        },
    )
    .await;

    let IntentOutcome::Failed { error, evidence } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::VisionAssistDenied);
    assert!(
        !called.load(Ordering::SeqCst),
        "propose must not be called when vision gates are closed"
    );
    let record = evidence.iter().find_map(|item| match item {
        Evidence::IntentExecution { record } => Some(record),
        _ => None,
    });
    let record = record.expect("stuck IntentExecution evidence");
    assert_eq!(record.verification, "targetNotFound");
}

#[tokio::test]
async fn stuck_with_gates_uses_vision_propose_and_execute() {
    let called = Arc::new(AtomicBool::new(false));
    let click_xy_calls = Arc::new(AtomicUsize::new(0));
    let assist = Arc::new(FakeVision {
        called: called.clone(),
        proposal: click_proposal(0.91),
    });
    let browser = FakeBrowser {
        screenshot_png: b"png".to_vec(),
        click_xy_calls: click_xy_calls.clone(),
        ..FakeBrowser::default()
    };
    let page_id = PageId::new();

    let outcome = IntentEngine::execute(
        &locate(),
        &page_id,
        &browser,
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(assist),
        },
    )
    .await;

    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    assert!(called.load(Ordering::SeqCst), "propose must be called");
    assert_eq!(click_xy_calls.load(Ordering::SeqCst), 1);
    let record = evidence.iter().find_map(|item| match item {
        Evidence::IntentExecution { record } => Some(record),
        _ => None,
    });
    let record = record.expect("IntentExecution evidence");
    assert_eq!(record.resolution_path, IntentResolutionPath::VisionFallback);
    assert!(record.vision_proposal_sha256.is_some());
    assert_eq!(record.verification, "visionFallback");
}

#[tokio::test]
async fn low_confidence_proposal_fails_closed() {
    let called = Arc::new(AtomicBool::new(false));
    let click_xy_calls = Arc::new(AtomicUsize::new(0));
    let assist = Arc::new(FakeVision {
        called: called.clone(),
        proposal: click_proposal(VISION_CONFIDENCE_FLOOR - 0.01),
    });
    let browser = FakeBrowser {
        screenshot_png: b"png".to_vec(),
        click_xy_calls: click_xy_calls.clone(),
        ..FakeBrowser::default()
    };
    let page_id = PageId::new();

    let outcome = IntentEngine::execute(
        &locate(),
        &page_id,
        &browser,
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(assist),
        },
    )
    .await;

    let IntentOutcome::Failed { error, .. } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::VisionAssistFailed);
    assert!(called.load(Ordering::SeqCst), "propose must be called once");
    assert_eq!(
        click_xy_calls.load(Ordering::SeqCst),
        0,
        "low confidence must not execute the proposal"
    );
}

#[tokio::test]
async fn policy_denied_never_calls_vision() {
    let called = Arc::new(AtomicBool::new(false));
    let assist = Arc::new(FakeVision {
        called: called.clone(),
        proposal: click_proposal(0.99),
    });
    let browser = FakeBrowser {
        gather_error: Some(CommandError {
            code: ErrorCode::PolicyDenied,
            message: "policy denied gather".into(),
            layer: types::ErrorLayer::Page,
            retryable: false,
        }),
        screenshot_png: b"png".to_vec(),
        ..FakeBrowser::default()
    };
    let page_id = PageId::new();

    let outcome = IntentEngine::execute(
        &locate(),
        &page_id,
        &browser,
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(assist),
        },
    )
    .await;

    let IntentOutcome::Failed { error, .. } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::PolicyDenied);
    assert!(
        !called.load(Ordering::SeqCst),
        "never_escalates(PolicyDenied) must not call vision"
    );
}
