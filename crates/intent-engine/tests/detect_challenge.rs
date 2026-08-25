use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use intent_engine::{
    IntentBrowser, IntentEngine, IntentOutcome, VisionAction, VisionAssist, VisionContext,
    VisionProposal, VisionProposeRequest,
};
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandError, DetectChallengeHints,
    DetectChallengeIntent, ErrorCode, Evidence, IntentCommand, PageId, TargetSpec, TypeTextCommand,
    UploadFilesCommand, WaitForCommand,
};

/// Replays proposals in order; once the script is exhausted the last
/// proposal repeats, so an uncooperative model can be simulated without an
/// infinite script. Captures every request purpose for prompt assertions.
struct ScriptedVision {
    proposals: Mutex<Vec<VisionProposal>>,
    calls: AtomicUsize,
    purposes: Mutex<Vec<String>>,
}

impl ScriptedVision {
    fn new(proposals: Vec<VisionProposal>) -> Arc<Self> {
        Arc::new(Self {
            proposals: Mutex::new(proposals),
            calls: AtomicUsize::new(0),
            purposes: Mutex::new(Vec::new()),
        })
    }

    fn purposes(&self) -> Vec<String> {
        self.purposes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

#[async_trait]
impl VisionAssist for ScriptedVision {
    async fn propose(&self, request: VisionProposeRequest) -> Result<VisionProposal, CommandError> {
        self.purposes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(request.purpose);
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let proposals = self.proposals.lock().unwrap_or_else(|p| p.into_inner());
        let index = call.min(proposals.len().saturating_sub(1));
        Ok(proposals[index].clone())
    }
}

#[derive(Default)]
struct FakeBrowser {
    screenshots: AtomicUsize,
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
        Err(unsupported("click_xy"))
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
        self.screenshots.fetch_add(1, Ordering::SeqCst);
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

fn detect(timeout_ms: u64) -> IntentCommand {
    IntentCommand::DetectChallenge(DetectChallengeIntent {
        purpose: "check for a captcha blocking signup".into(),
        hints: DetectChallengeHints {
            region: None,
            timeout_ms,
        },
    })
}

fn detected(kind: types::ChallengeType, blocking: bool) -> VisionProposal {
    VisionProposal {
        confidence: 0.92,
        action: VisionAction::ChallengeDetected {
            challenge_type: kind,
            region: None,
            blocking,
        },
    }
}

fn clean() -> VisionProposal {
    VisionProposal {
        confidence: 0.88,
        action: VisionAction::NoChallengeDetected,
    }
}

fn vision(assist: Arc<ScriptedVision>) -> VisionContext {
    VisionContext {
        session_ok: true,
        capability_ok: true,
        assist: Some(assist as Arc<dyn VisionAssist>),
        proposals: None,
        defer_escalation: false,
        prompt_context: None,
        corpus: None,
        context_store: None,
    }
}

fn detection_evidence(
    outcome: &IntentOutcome,
) -> Option<(Option<types::ChallengeDetection>, Option<String>)> {
    let IntentOutcome::Completed { evidence } = outcome else {
        return None;
    };
    evidence.iter().find_map(|item| match item {
        Evidence::ChallengeDetection {
            detection,
            prior_kind,
        } => Some((detection.clone(), prior_kind.clone())),
        _ => None,
    })
}

#[tokio::test]
async fn detect_challenge_requires_an_open_vision_gate() {
    let closed: Option<Arc<dyn VisionAssist>> = None;
    let cases = [
        (
            false,
            true,
            Some(ScriptedVision::new(vec![clean()]) as Arc<dyn VisionAssist>),
        ),
        (
            true,
            false,
            Some(ScriptedVision::new(vec![clean()]) as Arc<dyn VisionAssist>),
        ),
        (true, true, closed),
    ];
    for (session_ok, capability_ok, assist) in cases {
        let outcome = IntentEngine::execute(
            &detect(30_000),
            &PageId::new(),
            &FakeBrowser::default(),
            &VisionContext {
                session_ok,
                capability_ok,
                assist,
                proposals: None,
                defer_escalation: false,
                prompt_context: None,
                corpus: None,
                context_store: None,
            },
        )
        .await;
        let IntentOutcome::Failed { error, .. } = outcome else {
            panic!("detectChallenge without vision must fail");
        };
        assert_eq!(error.code, ErrorCode::VisionAssistDenied);
    }
}

#[tokio::test]
async fn detect_challenge_reports_a_detected_challenge() {
    let outcome = IntentEngine::execute(
        &detect(30_000),
        &PageId::new(),
        &FakeBrowser::default(),
        &vision(ScriptedVision::new(vec![detected(
            types::ChallengeType::RecaptchaV2Checkbox,
            true,
        )])),
    )
    .await;
    let Some((Some(detection), prior)) = detection_evidence(&outcome) else {
        panic!("expected detection evidence, got {outcome:?}");
    };
    assert_eq!(
        detection.challenge_type,
        types::ChallengeType::RecaptchaV2Checkbox
    );
    assert!(detection.blocking);
    assert!((detection.confidence - 0.92).abs() < f32::EPSILON);
    assert_eq!(prior, None);
}

#[tokio::test]
async fn detect_challenge_reports_a_clean_page_as_a_first_class_answer() {
    let outcome = IntentEngine::execute(
        &detect(30_000),
        &PageId::new(),
        &FakeBrowser::default(),
        &vision(ScriptedVision::new(vec![clean()])),
    )
    .await;
    let Some((detection, _)) = detection_evidence(&outcome) else {
        panic!("expected detection evidence, got {outcome:?}");
    };
    assert!(detection.is_none(), "a clean page is detection: None");
}

#[tokio::test]
async fn detect_challenge_reassesses_an_off_task_answer_instead_of_acting() {
    let browser = FakeBrowser::default();
    let outcome = IntentEngine::execute(
        &detect(30_000),
        &PageId::new(),
        &browser,
        &vision(ScriptedVision::new(vec![
            // The model first proposes a click — an action, not a
            // classification. The loop must not act on it.
            VisionProposal {
                confidence: 0.9,
                action: VisionAction::Click { x: 10.0, y: 10.0 },
            },
            clean(),
        ])),
    )
    .await;
    assert!(matches!(outcome, IntentOutcome::Completed { .. }));
    // Two rounds: the off-task dud plus the real classification.
    assert_eq!(browser.screenshots.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn detect_challenge_times_out_when_the_model_never_classifies() {
    let outcome = IntentEngine::execute(
        &detect(200),
        &PageId::new(),
        &FakeBrowser::default(),
        &vision(ScriptedVision::new(vec![VisionProposal {
            confidence: 0.9,
            action: VisionAction::Click { x: 1.0, y: 1.0 },
        }])),
    )
    .await;
    let IntentOutcome::Failed { error, .. } = outcome else {
        panic!("an unclassifiable page must hit the deadline");
    };
    assert_eq!(error.code, ErrorCode::DeadlineExceeded);
}

#[tokio::test]
async fn detect_challenge_enriches_the_prompt_with_the_site_prior() {
    let root = tempfile::tempdir().unwrap();
    let (store, _) = context_store::ContextStore::open(root.path(), "test")
        .await
        .unwrap();
    let url = "https://example.com/signup";
    let site = context_store::site_key(url).expect("site key");
    store
        .record_challenge(&site, "imageGrid", false, 20_000)
        .await;
    store
        .record_challenge(&site, "imageGrid", true, 20_000)
        .await;

    let assist = ScriptedVision::new(vec![detected(types::ChallengeType::ImageGridCaptcha, true)]);
    let mut context = vision(assist.clone());
    context.prompt_context = Some(intent_engine::VisionPromptContext {
        url: Some(url.into()),
        candidates: Vec::new(),
        recent_command_kinds: Vec::new(),
    });
    context.context_store = Some(Arc::new(store));

    let outcome = IntentEngine::execute(
        &detect(30_000),
        &PageId::new(),
        &FakeBrowser::default(),
        &context,
    )
    .await;
    assert!(matches!(outcome, IntentOutcome::Completed { .. }));
    let purposes = assist.purposes();
    assert_eq!(purposes.len(), 1);
    assert!(
        purposes[0].contains("check for a captcha blocking signup"),
        "caller's purpose stays intact: {}",
        purposes[0]
    );
    assert!(
        purposes[0].contains("imageGrid"),
        "site prior reaches the provider prompt: {}",
        purposes[0]
    );
    // The prior enriches the prompt but is also reported for transparency.
    let Some((_, Some(prior))) = detection_evidence(&outcome) else {
        panic!("expected the prior in detection evidence, got {outcome:?}");
    };
    assert_eq!(prior, "imageGrid");
}
