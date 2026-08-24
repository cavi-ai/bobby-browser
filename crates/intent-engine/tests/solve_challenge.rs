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
    proposals: Mutex<Vec<Result<VisionProposal, CommandError>>>,
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
        proposals[index].clone()
    }
}

#[derive(Default)]
struct FakeBrowser {
    click_xy_calls: AtomicUsize,
    type_text_calls: AtomicUsize,
    screenshot_failures: AtomicUsize,
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
        if self
            .screenshot_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(unsupported("capture_screenshot"));
        }
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
    vision_with_errors(script.into_iter().map(Ok).collect())
}

fn vision_with_errors(script: Vec<Result<VisionProposal, CommandError>>) -> VisionContext {
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
        context_store: None,
    }
}

#[tokio::test]
async fn solve_challenge_requires_an_open_vision_gate() {
    for vision in [
        VisionContext {
            session_ok: false,
            capability_ok: true,
            assist: Some(Arc::new(ScriptedVision {
                proposals: Mutex::new(vec![Ok(solved())]),
                calls: AtomicUsize::new(0),
            })),
            proposals: None,
            defer_escalation: false,
            prompt_context: None,
            corpus: None,
            context_store: None,
        },
        VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: None,
            proposals: None,
            defer_escalation: false,
            prompt_context: None,
            corpus: None,
            context_store: None,
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
async fn solve_challenge_retries_transient_duds_within_the_budget() {
    // A below-floor proposal and a provider error each cost one attempt;
    // the loop reassesses and can still complete.
    let mut low = solved();
    low.confidence = 0.5;
    let provider_error = CommandError {
        code: ErrorCode::VisionAssistFailed,
        message: "endpoint returned an invalid proposal".into(),
        layer: types::ErrorLayer::Driver,
        retryable: true,
    };
    let outcome = IntentEngine::execute(
        &solve(30_000),
        &PageId::new(),
        &FakeBrowser::default(),
        &vision_with_errors(vec![Ok(low), Err(provider_error), Ok(solved())]),
    )
    .await;
    assert!(matches!(outcome, IntentOutcome::Completed { .. }));
}

#[tokio::test]
async fn solve_challenge_retries_a_failed_screenshot() {
    // A renderer hiccup makes one capture fail while the page lives on;
    // the loop reassesses instead of dying.
    let browser = FakeBrowser {
        screenshot_failures: AtomicUsize::new(2),
        ..FakeBrowser::default()
    };
    let outcome = IntentEngine::execute(
        &solve(30_000),
        &PageId::new(),
        &browser,
        &vision(vec![solved()]),
    )
    .await;
    assert!(matches!(outcome, IntentOutcome::Completed { .. }));
}

#[tokio::test]
async fn solve_challenge_reports_transient_duds_when_the_deadline_wins() {
    let mut low = solved();
    low.confidence = 0.5;
    let outcome = IntentEngine::execute(
        &solve(50),
        &PageId::new(),
        &FakeBrowser::default(),
        &vision(vec![low]),
    )
    .await;
    let IntentOutcome::Failed { error, .. } = outcome else {
        panic!("a model that never gains confidence must hit the deadline");
    };
    assert_eq!(error.code, ErrorCode::DeadlineExceeded);
    assert!(error.message.contains("below floor"), "{error:?}");
}

#[tokio::test]
async fn solve_challenge_rejects_actions_it_cannot_ground() {
    for action in [
        VisionAction::ClickCandidate { index: 0 },
        // Typing needs a resolved target; a vision typeText carries none, and
        // the empty-selector act errors at the driver. Click-only for now.
        VisionAction::TypeText { text: "x".into() },
    ] {
        let candidate = VisionProposal {
            confidence: 0.9,
            action,
        };
        let outcome = IntentEngine::execute(
            &solve(30_000),
            &PageId::new(),
            &FakeBrowser::default(),
            &vision(vec![candidate]),
        )
        .await;
        let IntentOutcome::Failed { error, .. } = outcome else {
            panic!("non-click actions are not allowed for solveChallenge");
        };
        assert_eq!(error.code, ErrorCode::VisionAssistFailed);
    }
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

/// Captures every purpose the engine hands to the provider, then reports
/// solved so the loop ends on the first proposal.
struct PurposeProbe {
    purposes: Mutex<Vec<String>>,
}

#[async_trait]
impl VisionAssist for PurposeProbe {
    async fn propose(&self, request: VisionProposeRequest) -> Result<VisionProposal, CommandError> {
        self.purposes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(request.purpose);
        Ok(solved())
    }
}

#[tokio::test]
async fn solve_challenge_enriches_the_prompt_with_the_site_prior() {
    let root = tempfile::tempdir().unwrap();
    let (store, _) = context_store::ContextStore::open(root.path(), "test")
        .await
        .unwrap();
    let url = "https://example.com/signup";
    let site = context_store::site_key(url).expect("site key");
    store
        .record_challenge(&site, "recaptcha", true, 20_000)
        .await;

    let probe = Arc::new(PurposeProbe {
        purposes: Mutex::new(Vec::new()),
    });
    let vision = VisionContext {
        session_ok: true,
        capability_ok: true,
        assist: Some(probe.clone()),
        proposals: None,
        defer_escalation: false,
        prompt_context: Some(intent_engine::VisionPromptContext {
            url: Some(url.into()),
            candidates: Vec::new(),
            recent_command_kinds: Vec::new(),
        }),
        corpus: None,
        context_store: Some(Arc::new(store)),
    };
    let outcome = IntentEngine::execute(
        &solve(30_000),
        &PageId::new(),
        &FakeBrowser::default(),
        &vision,
    )
    .await;
    assert!(matches!(outcome, IntentOutcome::Completed { .. }));
    let purposes = probe.purposes.lock().unwrap_or_else(|p| p.into_inner());
    assert_eq!(purposes.len(), 1);
    assert!(
        purposes[0].contains("solve the reCAPTCHA challenge"),
        "caller's purpose stays intact: {}",
        purposes[0]
    );
    assert!(
        purposes[0].contains("recaptcha"),
        "site prior reaches the provider prompt: {}",
        purposes[0]
    );
}

#[tokio::test]
async fn solve_challenge_without_a_store_leaves_the_prompt_alone() {
    let probe = Arc::new(PurposeProbe {
        purposes: Mutex::new(Vec::new()),
    });
    let mut vision = vision_with_errors(vec![Ok(solved())]);
    vision.assist = Some(probe.clone());
    vision.prompt_context = Some(intent_engine::VisionPromptContext {
        url: Some("https://example.com/signup".into()),
        candidates: Vec::new(),
        recent_command_kinds: Vec::new(),
    });
    let outcome = IntentEngine::execute(
        &solve(30_000),
        &PageId::new(),
        &FakeBrowser::default(),
        &vision,
    )
    .await;
    assert!(matches!(outcome, IntentOutcome::Completed { .. }));
    let purposes = probe.purposes.lock().unwrap_or_else(|p| p.into_inner());
    assert_eq!(purposes.as_slice(), ["solve the reCAPTCHA challenge"]);
}
