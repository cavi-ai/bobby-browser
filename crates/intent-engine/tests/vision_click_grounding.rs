use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dom_engine::{Candidate, CandidateState};
use intent_engine::{
    IntentBrowser, IntentEngine, IntentOutcome, VisionAction, VisionAssist, VisionContext,
    VisionProposal, VisionProposeRequest,
};
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandError, ErrorCode, Evidence, IntentCommand,
    IntentHints, LocateIntent, PageId, TargetSpec, TypeTextCommand, UploadFilesCommand,
    WaitForCommand,
};

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

struct Control {
    x: std::ops::Range<f64>,
    y: std::ops::Range<f64>,
    role: &'static str,
    name: &'static str,
}

fn control(
    x: std::ops::Range<f64>,
    y: std::ops::Range<f64>,
    role: &'static str,
    name: &'static str,
) -> Control {
    Control { x, y, role, name }
}

#[derive(Default)]
struct FakeBrowser {
    controls: Vec<Control>,
    background: Option<(&'static str, &'static str)>,
    lookup_unsupported: bool,
    clicks: Arc<Mutex<Vec<(f64, f64)>>>,
}

#[async_trait]
impl IntentBrowser for FakeBrowser {
    async fn collect_candidates(
        &self,
        _page_id: &PageId,
        _target: &TargetSpec,
    ) -> Result<Vec<Candidate>, CommandError> {
        Ok(vec![candidate("button", "Continue")])
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
        x: f64,
        y: f64,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.clicks.lock().unwrap().push((x, y));
        Ok(vec![])
    }

    async fn element_at_point(
        &self,
        _page_id: &PageId,
        x: f64,
        y: f64,
    ) -> Result<Option<(String, String)>, CommandError> {
        if self.lookup_unsupported {
            return Err(unsupported("element_at_point"));
        }
        Ok(self
            .controls
            .iter()
            .find(|c| c.x.contains(&x) && c.y.contains(&y))
            .map(|c| (c.role, c.name))
            .or(self.background)
            .map(|(role, name)| (role.to_string(), name.to_string())))
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
        Ok((b"png-bytes".to_vec(), vec![]))
    }

    async fn capture_sanitized_screenshot(
        &self,
        _page_id: &PageId,
    ) -> Result<Vec<u8>, CommandError> {
        Ok(b"sanitized-png-bytes".to_vec())
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

fn candidate(role: &str, name: &str) -> Candidate {
    Candidate {
        id: name.into(),
        css: Some(format!("[aria-label='{name}']")),
        tag: None,
        test_id: None,
        role: Some(role.into()),
        name: Some(name.into()),
        label: None,
        text: name.into(),
        attributes: BTreeMap::new(),
        state: CandidateState {
            attached: true,
            visible: true,
            enabled: true,
        },
        frame_path: Vec::new(),
    }
}

fn unresolvable_locate() -> IntentCommand {
    IntentCommand::Locate(LocateIntent {
        purpose: "Continue to checkout".into(),
        hints: IntentHints {
            role: Some("link".into()),
            ..IntentHints::default()
        },
    })
}

async fn run_click(browser: &FakeBrowser, x: f64, y: f64) -> IntentOutcome {
    let assist = Arc::new(FakeVision {
        proposal: VisionProposal {
            confidence: 0.98,
            action: VisionAction::Click { x, y },
        },
    });
    IntentEngine::execute(
        &unresolvable_locate(),
        &PageId::new(),
        browser,
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(assist),
            proposals: None,
            defer_escalation: false,
            prompt_context: None,
            corpus: None,
            context_store: None,
        },
    )
    .await
}

fn clicks(browser: &FakeBrowser) -> Vec<(f64, f64)> {
    browser.clicks.lock().unwrap().clone()
}

#[tokio::test]
async fn near_miss_click_lands_on_the_only_nearby_control() {
    let browser = FakeBrowser {
        controls: vec![control(100.0..180.0, 50.0..90.0, "link", "Join now")],
        background: Some(("", "page text")),
        ..FakeBrowser::default()
    };
    assert_lands_in_control(&browser).await;
}

#[tokio::test]
async fn near_miss_on_empty_space_lands_on_the_only_nearby_control() {
    let browser = FakeBrowser {
        controls: vec![control(100.0..180.0, 50.0..90.0, "link", "Join now")],
        ..FakeBrowser::default()
    };
    assert_lands_in_control(&browser).await;
}

async fn assert_lands_in_control(browser: &FakeBrowser) {
    let outcome = run_click(browser, 95.0, 70.0).await;
    assert!(
        matches!(outcome, IntentOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let points = clicks(browser);
    assert_eq!(points.len(), 1);
    let (x, y) = points[0];
    assert!((100.0..180.0).contains(&x) && (50.0..90.0).contains(&y));
}

#[tokio::test]
async fn control_with_an_unlisted_role_does_not_attract_the_click() {
    let browser = FakeBrowser {
        controls: vec![control(100.0..180.0, 50.0..90.0, "heading", "Title")],
        ..FakeBrowser::default()
    };
    let outcome = run_click(&browser, 95.0, 70.0).await;
    assert!(
        matches!(outcome, IntentOutcome::Completed { .. }),
        "{outcome:?}"
    );
    assert_eq!(clicks(&browser), vec![(95.0, 70.0)]);
}

#[tokio::test]
async fn click_between_two_controls_is_declined() {
    let browser = FakeBrowser {
        controls: vec![
            control(100.0..180.0, 50.0..65.0, "link", "User Agreement"),
            control(100.0..180.0, 66.0..81.0, "link", "Privacy Policy"),
        ],
        background: Some(("", "page text")),
        ..FakeBrowser::default()
    };
    let outcome = run_click(&browser, 140.0, 65.5).await;
    let IntentOutcome::Failed { error, .. } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::VisionAssistFailed);
    assert!(clicks(&browser).is_empty());
}

#[tokio::test]
async fn click_on_a_control_is_unchanged() {
    let browser = FakeBrowser {
        controls: vec![control(100.0..180.0, 50.0..90.0, "link", "Join now")],
        ..FakeBrowser::default()
    };
    let outcome = run_click(&browser, 140.0, 70.0).await;
    assert!(
        matches!(outcome, IntentOutcome::Completed { .. }),
        "{outcome:?}"
    );
    assert_eq!(clicks(&browser), vec![(140.0, 70.0)]);
}

#[tokio::test]
async fn click_far_from_every_control_is_unchanged() {
    let browser = FakeBrowser {
        controls: vec![control(300.0..380.0, 50.0..90.0, "link", "Join now")],
        ..FakeBrowser::default()
    };
    let outcome = run_click(&browser, 95.0, 70.0).await;
    assert!(
        matches!(outcome, IntentOutcome::Completed { .. }),
        "{outcome:?}"
    );
    assert_eq!(clicks(&browser), vec![(95.0, 70.0)]);
}

#[tokio::test]
async fn unsupported_point_lookup_is_unchanged() {
    let browser = FakeBrowser {
        lookup_unsupported: true,
        ..FakeBrowser::default()
    };
    let outcome = run_click(&browser, 95.0, 70.0).await;
    assert!(
        matches!(outcome, IntentOutcome::Completed { .. }),
        "{outcome:?}"
    );
    assert_eq!(clicks(&browser), vec![(95.0, 70.0)]);
}
