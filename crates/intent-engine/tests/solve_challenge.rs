use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use intent_engine::{
    IntentBrowser, IntentEngine, IntentOutcome, VisionAction, VisionAssist, VisionContext,
    VisionProposal, VisionProposeRequest,
};
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandError, ErrorCode, Evidence, IntentCommand,
    PageId, SolveChallengeHints, SolveChallengeIntent, TargetSpec, TypeTextCommand,
    UploadFilesCommand, WaitForCommand,
};

/// Replays proposals in order; once the script is exhausted the last
/// proposal repeats, so an uncooperative model can be simulated without an
/// infinite script.
struct ScriptedVision {
    proposals: Mutex<Vec<VisionProposal>>,
    calls: AtomicUsize,
}

#[async_trait]
impl VisionAssist for ScriptedVision {
    async fn propose(
        &self,
        _request: VisionProposeRequest,
    ) -> Result<VisionProposal, CommandError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let proposals = self.proposals.lock().unwrap_or_else(|p| p.into_inner());
        let index = call.min(proposals.len().saturating_sub(1));
        Ok(proposals[index].clone())
    }
}

#[derive(Default)]
struct FakeBrowser {
    click_xy_calls: AtomicUsize,
    type_text_calls: AtomicUsize,
}

#[async_trait]
impl IntentBrowser for FakeBrowser {
    async fn collect_candidates(
        &self,
        _page_id: &PageId,
        _target: &TargetSpec,
    ) -> Result<Vec<dom_engine::Candidate>, CommandError> {
        Ok(Vec::new())
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
        self.type_text_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
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
            b"png".to_vec(),
            vec![Evidence::Screenshot {
                artifact_id: "shot-1".into(),
                media_type: "image/png".into(),
                width: 1,
                height: 1,
                bytes: 3,
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

fn solve(timeout_ms: u64) -> IntentCommand {
    IntentCommand::SolveChallenge(SolveChallengeIntent {
        purpose: "solve the reCAPTCHA challenge".into(),
        hints: SolveChallengeHints {
            region: None,
            timeout_ms,
        },
    })
}

fn solved() -> VisionProposal {
    VisionProposal {
        confidence: 0.9,
        action: VisionAction::ChallengeSolved,
    }
}

fn click() -> VisionProposal {
    VisionProposal {
        confidence: 0.9,
        action: VisionAction::Click { x: 12.0, y: 34.0 },
    }
}

fn vision(script: Vec<VisionProposal>) -> VisionContext {
    VisionContext {
        session_ok: true,
        capability_ok: true,
        assist: Some(Arc::new(ScriptedVision {
            proposals: Mutex::new(script),
            calls: AtomicUsize::new(0),
        })),
        proposals: None,
        defer_escalation: false,
        prompt_context: None,
        corpus: None,
    }
}

#[tokio::test]
async fn solve_challenge_requires_an_open_vision_gate() {
    for vision in [
        VisionContext {
            session_ok: false,
            capability_ok: true,
            assist: Some(Arc::new(ScriptedVision {
                proposals: Mutex::new(vec![solved()]),
                calls: AtomicUsize::new(0),
            })),
            proposals: None,
            defer_escalation: false,
            prompt_context: None,
            corpus: None,
        },
        VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: None,
            proposals: None,
            defer_escalation: false,
            prompt_context: None,
            corpus: None,
        },
    ] {
        let outcome = IntentEngine::execute(
            &solve(30_000),
            &PageId::new(),
            &FakeBrowser::default(),
            &vision,
        )
        .await;
        let IntentOutcome::Failed { error, .. } = outcome else {
            panic!("solveChallenge without vision must fail");
        };
        assert_eq!(error.code, ErrorCode::VisionAssistDenied);
    }
}

#[tokio::test]
async fn solve_challenge_completes_when_the_model_reports_solved() {
    let outcome = IntentEngine::execute(
        &solve(30_000),
        &PageId::new(),
        &FakeBrowser::default(),
        &vision(vec![solved()]),
    )
    .await;
    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("challengeSolved must complete the intent");
    };
    let record = evidence.iter().find_map(|item| match item {
        Evidence::IntentExecution { record } => Some(record),
        _ => None,
    });
    let record = record.expect("terminal intent execution record");
    assert_eq!(record.intent_kind, "solveChallenge");
    assert!(record.verification.starts_with("challengeSolved"));
    assert_eq!(
        record.resolution_path,
        types::IntentResolutionPath::VisionFallback
    );
}

#[tokio::test]
async fn solve_challenge_acts_then_reassesses_until_solved() {
    let browser = FakeBrowser::default();
    let outcome = IntentEngine::execute(
        &solve(30_000),
        &PageId::new(),
        &browser,
        &vision(vec![click(), solved()]),
    )
    .await;
    assert!(matches!(outcome, IntentOutcome::Completed { .. }));
    assert_eq!(browser.click_xy_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn solve_challenge_fails_closed_below_the_confidence_floor() {
    let mut low = solved();
    low.confidence = 0.5;
    let outcome = IntentEngine::execute(
        &solve(30_000),
        &PageId::new(),
        &FakeBrowser::default(),
        &vision(vec![low]),
    )
    .await;
    let IntentOutcome::Failed { error, .. } = outcome else {
        panic!("below-floor proposal must fail closed");
    };
    assert_eq!(error.code, ErrorCode::VisionAssistFailed);
}

#[tokio::test]
async fn solve_challenge_rejects_actions_it_cannot_ground() {
    let candidate = VisionProposal {
        confidence: 0.9,
        action: VisionAction::ClickCandidate { index: 0 },
    };
    let outcome = IntentEngine::execute(
        &solve(30_000),
        &PageId::new(),
        &FakeBrowser::default(),
        &vision(vec![candidate]),
    )
    .await;
    let IntentOutcome::Failed { error, .. } = outcome else {
        panic!("candidate actions are not allowed for solveChallenge");
    };
    assert_eq!(error.code, ErrorCode::VisionAssistFailed);
}

#[tokio::test]
async fn solve_challenge_times_out_when_the_model_never_solves() {
    let outcome = IntentEngine::execute(
        &solve(50),
        &PageId::new(),
        &FakeBrowser::default(),
        &vision(vec![click()]),
    )
    .await;
    let IntentOutcome::Failed { error, .. } = outcome else {
        panic!("an unsolved challenge must hit the deadline");
    };
    assert_eq!(error.code, ErrorCode::DeadlineExceeded);
}
