use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dom_engine::{Candidate, CandidateState};
use intent_engine::{
    IntentBrowser, IntentEngine, IntentOutcome, VisionAction, VisionAssist, VisionContext,
    VisionProposal, VisionProposeRequest,
};
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandError, DismissObstructionIntent, ErrorCode,
    Evidence, IntentCommand, IntentHints, IntentResolutionPath, PageId, TargetSpec,
    TypeTextCommand, UploadFilesCommand, WaitForCommand,
};

#[derive(Default)]
struct CallLog {
    clicks: Vec<ClickCommand>,
}

#[derive(Default)]
struct FakeBrowser {
    /// Each `collect_candidates` call pops the next state in the sequence;
    /// the last entry repeats once the sequence is exhausted, modeling a DOM
    /// that has settled into a final state.
    candidate_sequence: Arc<Mutex<VecDeque<Vec<Candidate>>>>,
    calls: Arc<Mutex<CallLog>>,
    click_evidence: Vec<Evidence>,
    screenshot_png: Vec<u8>,
    click_xy_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl IntentBrowser for FakeBrowser {
    async fn collect_candidates(
        &self,
        _page_id: &PageId,
        _target: &TargetSpec,
    ) -> Result<Vec<Candidate>, CommandError> {
        let mut sequence = self.candidate_sequence.lock().expect("sequence");
        if sequence.len() > 1 {
            Ok(sequence.pop_front().unwrap_or_default())
        } else {
            Ok(sequence.front().cloned().unwrap_or_default())
        }
    }

    async fn click(
        &self,
        _page_id: &PageId,
        command: &ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.calls
            .lock()
            .expect("call log")
            .clicks
            .push(command.clone());
        Ok(self.click_evidence.clone())
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

struct FakeVision {
    proposal: VisionProposal,
}

#[async_trait]
impl VisionAssist for FakeVision {
    async fn propose(
        &self,
        _request: VisionProposeRequest,
    ) -> Result<VisionProposal, CommandError> {
        Ok(self.proposal.clone())
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

fn overlay_close_button() -> Candidate {
    Candidate {
        id: "dismiss-overlay".into(),
        css: Some("#dismiss-overlay".into()),
        test_id: None,
        role: Some("button".into()),
        name: Some("Close".into()),
        label: None,
        text: "Close".into(),
        attributes: BTreeMap::new(),
        state: CandidateState {
            attached: true,
            visible: true,
            enabled: true,
        },
        frame_path: Vec::new(),
    }
}

fn still_present() -> Candidate {
    Candidate {
        state: CandidateState {
            attached: true,
            visible: true,
            enabled: true,
        },
        ..overlay_close_button()
    }
}

fn hidden_but_attached() -> Candidate {
    Candidate {
        state: CandidateState {
            attached: true,
            visible: false,
            enabled: true,
        },
        ..overlay_close_button()
    }
}

const CLOSE_BUTTON_PURPOSE: &str = "Close";

fn dismiss(purpose: &str, role: Option<&str>, timeout_ms: u64) -> IntentCommand {
    IntentCommand::DismissObstruction(DismissObstructionIntent {
        purpose: purpose.into(),
        hints: IntentHints {
            role: role.map(str::to_owned),
            ..IntentHints::default()
        },
        timeout_ms,
    })
}

#[tokio::test]
async fn dismiss_clicks_close_control_then_completes_once_it_is_removed_from_the_dom() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let browser = FakeBrowser {
        candidate_sequence: Arc::new(Mutex::new(VecDeque::from([vec![still_present()], vec![]]))),
        calls: Arc::clone(&calls),
        click_evidence: vec![Evidence::Element {
            selector: "#dismiss-overlay".into(),
            text: None,
        }],
        ..FakeBrowser::default()
    };
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &dismiss(CLOSE_BUTTON_PURPOSE, Some("button"), 500),
        &page_id,
        &browser,
        &VisionContext::default(),
    )
    .await;

    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    {
        let log = calls.lock().expect("call log");
        assert_eq!(log.clicks.len(), 1);
        assert!(
            !log.clicks[0].boundary,
            "DismissObstructionIntent has no boundary flag; click must never be boundary"
        );
    }
    let record = evidence.iter().find_map(|item| match item {
        Evidence::IntentExecution { record } => Some(record),
        _ => None,
    });
    let record = record.expect("IntentExecution evidence");
    assert_eq!(record.intent_kind, "dismissObstruction");
    assert_eq!(record.purpose.as_deref(), Some(CLOSE_BUTTON_PURPOSE));
    assert_eq!(record.resolution_path, IntentResolutionPath::Deterministic);
    assert_eq!(record.verification, "dismissed");
    assert!(evidence
        .iter()
        .any(|item| matches!(item, Evidence::Resolution { .. })));
}

#[tokio::test]
async fn dismiss_completes_when_target_becomes_hidden_but_stays_attached() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let browser = FakeBrowser {
        candidate_sequence: Arc::new(Mutex::new(VecDeque::from([
            vec![still_present()],
            vec![hidden_but_attached()],
        ]))),
        calls: Arc::clone(&calls),
        click_evidence: Vec::new(),
        ..FakeBrowser::default()
    };
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &dismiss(CLOSE_BUTTON_PURPOSE, Some("button"), 500),
        &page_id,
        &browser,
        &VisionContext::default(),
    )
    .await;

    assert!(matches!(outcome, IntentOutcome::Completed { .. }));
}

#[tokio::test]
async fn dismiss_missing_target_is_stuck_without_vision_configured() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let browser = FakeBrowser {
        candidate_sequence: Arc::new(Mutex::new(VecDeque::from([Vec::new()]))),
        calls: Arc::clone(&calls),
        click_evidence: Vec::new(),
        ..FakeBrowser::default()
    };
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &dismiss(CLOSE_BUTTON_PURPOSE, Some("button"), 500),
        &page_id,
        &browser,
        &VisionContext::default(),
    )
    .await;

    let IntentOutcome::Failed { error, evidence } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::VisionAssistDenied);
    {
        let log = calls.lock().expect("call log");
        assert!(log.clicks.is_empty());
    }
    let record = evidence.iter().find_map(|item| match item {
        Evidence::IntentExecution { record } => Some(record),
        _ => None,
    });
    let record = record.expect("IntentExecution on stuck");
    assert_eq!(record.verification, "targetNotFound");
    assert_eq!(record.intent_kind, "dismissObstruction");
}

#[tokio::test]
async fn dismiss_still_present_after_click_is_obstruction_suspected_without_vision_configured() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let browser = FakeBrowser {
        // Never leaves the "still present" state within the bounded timeout.
        candidate_sequence: Arc::new(Mutex::new(VecDeque::from([vec![still_present()]]))),
        calls: Arc::clone(&calls),
        click_evidence: vec![Evidence::Element {
            selector: "#dismiss-overlay".into(),
            text: None,
        }],
        ..FakeBrowser::default()
    };
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &dismiss(CLOSE_BUTTON_PURPOSE, Some("button"), 60),
        &page_id,
        &browser,
        &VisionContext::default(),
    )
    .await;

    let IntentOutcome::Failed { error, evidence } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::VisionAssistDenied);
    {
        let log = calls.lock().expect("call log");
        assert_eq!(
            log.clicks.len(),
            1,
            "the close control must have been clicked before giving up"
        );
    }
    // Evidence from the click that did happen must not be lost even though
    // the intent ultimately failed.
    assert!(
        evidence
            .iter()
            .any(|item| matches!(item, Evidence::Resolution { .. })),
        "resolution evidence from before the stuck check must be preserved"
    );
    let record = evidence.iter().find_map(|item| match item {
        Evidence::IntentExecution { record } => Some(record),
        _ => None,
    });
    let record = record.expect("IntentExecution on stuck");
    assert_eq!(record.verification, "obstructionPersisted");
    assert_eq!(record.intent_kind, "dismissObstruction");
}

#[tokio::test]
async fn dismiss_still_present_after_click_escalates_to_vision_and_preserves_prior_evidence() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let click_xy_calls = Arc::new(AtomicUsize::new(0));
    let browser = FakeBrowser {
        candidate_sequence: Arc::new(Mutex::new(VecDeque::from([vec![still_present()]]))),
        calls: Arc::clone(&calls),
        click_evidence: vec![Evidence::Element {
            selector: "#dismiss-overlay".into(),
            text: None,
        }],
        screenshot_png: b"png".to_vec(),
        click_xy_calls: click_xy_calls.clone(),
    };
    let assist = Arc::new(FakeVision {
        proposal: VisionProposal {
            confidence: 0.9,
            action: VisionAction::Click { x: 5.0, y: 6.0 },
        },
    });
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &dismiss(CLOSE_BUTTON_PURPOSE, Some("button"), 60),
        &page_id,
        &browser,
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(assist),
            proposals: None,
            defer_escalation: false,
            prompt_context: None,
        },
    )
    .await;

    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    assert_eq!(click_xy_calls.load(Ordering::SeqCst), 1);
    {
        let log = calls.lock().expect("call log");
        assert_eq!(
            log.clicks.len(),
            1,
            "the deterministic close-control click must still have happened"
        );
    }
    assert!(
        evidence
            .iter()
            .any(|item| matches!(item, Evidence::Resolution { .. })),
        "resolution evidence from the deterministic attempt must be preserved through vision fallback"
    );
    let record = evidence.iter().find_map(|item| match item {
        Evidence::IntentExecution { record } => Some(record),
        _ => None,
    });
    let record = record.expect("IntentExecution evidence");
    assert_eq!(record.resolution_path, IntentResolutionPath::VisionFallback);
    assert_eq!(record.verification, "visionFallback");
}
