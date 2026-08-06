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
            proposals: None,
            defer_escalation: false,
            prompt_context: None,
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
            proposals: None,
            defer_escalation: false,
            prompt_context: None,
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
            proposals: None,
            defer_escalation: false,
            prompt_context: None,
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
            proposals: None,
            defer_escalation: false,
            prompt_context: None,
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

/// C4: an open session policy does not substitute for the capability.
///
/// This is the row the node substrate makes load-bearing. A session names a
/// node and sets `executionPolicy.visionAssist`, both of which it controls;
/// the capability comes from the bearer token, which it does not. If an open
/// session grant were enough, naming a node would be a way to reach vision
/// with a token that never carried `vision:assist`.
///
/// Asserted by call count, not by error code: an assertion on the code alone
/// would pass even if the provider had been consulted and its answer then
/// discarded, which is a different security story from never asking.
#[tokio::test]
async fn an_open_session_policy_does_not_substitute_for_the_capability() {
    let called = Arc::new(AtomicBool::new(false));
    let assist = Arc::new(FakeVision {
        called: called.clone(),
        proposal: click_proposal(0.99),
    });
    let browser = FakeBrowser {
        screenshot_png: b"png".to_vec(),
        ..FakeBrowser::default()
    };

    let outcome = IntentEngine::execute(
        &locate(),
        &PageId::new(),
        &browser,
        &VisionContext {
            session_ok: true,
            capability_ok: false,
            assist: Some(assist),
            proposals: None,
            defer_escalation: false,
            prompt_context: None,
        },
    )
    .await;

    assert!(
        !called.load(Ordering::SeqCst),
        "an open session policy reached the vision provider without the capability"
    );
    let IntentOutcome::Failed { error, .. } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert_eq!(
        error.code,
        ErrorCode::VisionAssistDenied,
        "a missing capability was not reported as a denial"
    );
}

/// C4, the mirror: holding the capability does not substitute for the session
/// grant. Without this the double gate would be a single gate wearing two
/// names.
#[tokio::test]
async fn holding_the_capability_does_not_substitute_for_the_session_grant() {
    let called = Arc::new(AtomicBool::new(false));
    let assist = Arc::new(FakeVision {
        called: called.clone(),
        proposal: click_proposal(0.99),
    });
    let browser = FakeBrowser {
        screenshot_png: b"png".to_vec(),
        ..FakeBrowser::default()
    };

    let outcome = IntentEngine::execute(
        &locate(),
        &PageId::new(),
        &browser,
        &VisionContext {
            session_ok: false,
            capability_ok: true,
            assist: Some(assist),
            proposals: None,
            defer_escalation: false,
            prompt_context: None,
        },
    )
    .await;

    assert!(
        !called.load(Ordering::SeqCst),
        "the capability alone reached the vision provider without the session grant"
    );
    let IntentOutcome::Failed { error, .. } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::VisionAssistDenied);
}

#[derive(Default)]
struct FakeProposals {
    hits: std::collections::HashMap<String, intent_engine::CachedProposal>,
    consulted: Arc<AtomicBool>,
    dropped: Arc<AtomicUsize>,
}

impl intent_engine::ProposalLookup for FakeProposals {
    fn proposal_for(&self, _page: &PageId, purpose: &str) -> Option<intent_engine::CachedProposal> {
        self.consulted.store(true, Ordering::SeqCst);
        self.hits
            .get(purpose.trim().to_lowercase().as_str())
            .cloned()
    }
    fn drop_proposal(&self, _page: &PageId, _purpose: &str) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
    fn record_proposals(
        &self,
        _page: &PageId,
        _proposals: Vec<(String, intent_engine::CachedProposal)>,
    ) {
    }
}

fn cached_click(confidence: f32) -> intent_engine::CachedProposal {
    intent_engine::CachedProposal {
        x: 12.0,
        y: 34.0,
        confidence,
    }
}

#[tokio::test]
async fn cache_hit_never_calls_the_provider() {
    let called = Arc::new(AtomicBool::new(false));
    let click_xy_calls = Arc::new(AtomicUsize::new(0));
    let assist = Arc::new(FakeVision {
        called: called.clone(),
        proposal: click_proposal(0.91),
    });
    let proposals = Arc::new(FakeProposals {
        hits: [("continue".to_string(), cached_click(0.9))]
            .into_iter()
            .collect(),
        consulted: Arc::new(AtomicBool::new(false)),
        dropped: Arc::new(AtomicUsize::new(0)),
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
            proposals: Some(proposals),
            defer_escalation: false,
            prompt_context: None,
        },
    )
    .await;

    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    assert!(
        !called.load(Ordering::SeqCst),
        "provider was called despite a cache hit"
    );
    assert_eq!(click_xy_calls.load(Ordering::SeqCst), 1);
    let record = evidence.iter().find_map(|item| match item {
        Evidence::IntentExecution { record } => Some(record),
        _ => None,
    });
    let record = record.expect("IntentExecution evidence");
    assert_eq!(record.resolution_path, IntentResolutionPath::VisionPrefill);
    assert_eq!(record.verification, "visionPrefill");
}

#[tokio::test]
async fn cache_miss_falls_through_to_live_escalation() {
    let called = Arc::new(AtomicBool::new(false));
    let assist = Arc::new(FakeVision {
        called: called.clone(),
        proposal: click_proposal(0.91),
    });
    let proposals = Arc::new(FakeProposals::default());
    let consulted = proposals.consulted.clone();
    let browser = FakeBrowser {
        screenshot_png: b"png".to_vec(),
        click_xy_calls: Arc::new(AtomicUsize::new(0)),
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
            proposals: Some(proposals),
            defer_escalation: false,
            prompt_context: None,
        },
    )
    .await;

    let IntentOutcome::Completed { .. } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    assert!(
        consulted.load(Ordering::SeqCst),
        "cache was never consulted"
    );
    assert!(
        called.load(Ordering::SeqCst),
        "provider was not called on a miss"
    );
}

#[tokio::test]
async fn closed_gates_never_consult_the_cache() {
    for (session_ok, capability_ok) in [(true, false), (false, true)] {
        let proposals = Arc::new(FakeProposals {
            hits: [("continue".to_string(), cached_click(0.9))]
                .into_iter()
                .collect(),
            consulted: Arc::new(AtomicBool::new(false)),
            dropped: Arc::new(AtomicUsize::new(0)),
        });
        let consulted = proposals.consulted.clone();
        let browser = FakeBrowser::default();
        let page_id = PageId::new();

        let outcome = IntentEngine::execute(
            &locate(),
            &page_id,
            &browser,
            &VisionContext {
                session_ok,
                capability_ok,
                assist: None,
                proposals: Some(proposals),
                defer_escalation: false,
                prompt_context: None,
            },
        )
        .await;

        let IntentOutcome::Failed { .. } = outcome else {
            panic!("expected Failed, got {outcome:?}");
        };
        assert!(
            !consulted.load(Ordering::SeqCst),
            "cache consulted with gates closed ({session_ok}, {capability_ok})"
        );
    }
}

#[tokio::test]
async fn a_failed_cached_proposal_is_dropped_and_escalates_live() {
    let called = Arc::new(AtomicBool::new(false));
    let assist = Arc::new(FakeVision {
        called: called.clone(),
        proposal: click_proposal(0.91),
    });
    let proposals = Arc::new(FakeProposals {
        hits: [("continue".to_string(), cached_click(0.9))]
            .into_iter()
            .collect(),
        consulted: Arc::new(AtomicBool::new(false)),
        dropped: Arc::new(AtomicUsize::new(0)),
    });
    let dropped = proposals.dropped.clone();
    let browser = FakeBrowser {
        screenshot_png: b"png".to_vec(),
        // click_xy fails for the cached click, succeeds for the live one?
        // FakeBrowser::click_xy always succeeds here, so use a failing
        // override via gather_error-free path: simulate by flag.
        click_xy_calls: Arc::new(AtomicUsize::new(0)),
        ..FakeBrowser::default()
    };
    let _ = browser;
    // Use a browser whose click_xy fails to force the drop path.
    let browser = FailingClickBrowser {
        inner: FakeBrowser {
            screenshot_png: b"png".to_vec(),
            ..FakeBrowser::default()
        },
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
            proposals: Some(proposals),
            defer_escalation: false,
            prompt_context: None,
        },
    )
    .await;

    assert_eq!(
        dropped.load(Ordering::SeqCst),
        1,
        "bad entry was not dropped"
    );
    assert!(
        called.load(Ordering::SeqCst),
        "live escalation did not run after a failed cached proposal"
    );
    let IntentOutcome::Failed { .. } = outcome else {
        // Live escalation runs click_xy again, which fails here too — the
        // outcome is a vision act failure, which is correct and expected.
        panic!("expected Failed from the failing live act, got {outcome:?}");
    };
}

/// A browser whose click_xy always fails, to exercise the cached-proposal
/// drop path; everything else delegates to FakeBrowser.
struct FailingClickBrowser {
    inner: FakeBrowser,
}

#[async_trait]
impl IntentBrowser for FailingClickBrowser {
    async fn collect_candidates(
        &self,
        page_id: &PageId,
        target: &TargetSpec,
    ) -> Result<Vec<dom_engine::Candidate>, CommandError> {
        self.inner.collect_candidates(page_id, target).await
    }
    async fn click(
        &self,
        page_id: &PageId,
        command: &ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.inner.click(page_id, command).await
    }
    async fn click_xy(
        &self,
        _page_id: &PageId,
        _x: f64,
        _y: f64,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(CommandError {
            code: ErrorCode::VisionAssistFailed,
            message: "click_xy failed".into(),
            layer: types::ErrorLayer::Page,
            retryable: false,
        })
    }
    async fn type_text(
        &self,
        page_id: &PageId,
        command: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.inner.type_text(page_id, command).await
    }
    async fn upload_files(
        &self,
        page_id: &PageId,
        command: &UploadFilesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.inner.upload_files(page_id, command).await
    }
    async fn wait_for(
        &self,
        page_id: &PageId,
        command: &WaitForCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.inner.wait_for(page_id, command).await
    }
    async fn capture_screenshot(
        &self,
        page_id: &PageId,
        command: &CaptureScreenshotCommand,
    ) -> Result<(Vec<u8>, Vec<Evidence>), CommandError> {
        self.inner.capture_screenshot(page_id, command).await
    }
}

struct CountingVision {
    propose_calls: Arc<AtomicUsize>,
    confidence: f32,
}

#[async_trait]
impl VisionAssist for CountingVision {
    async fn propose(
        &self,
        _request: VisionProposeRequest,
    ) -> Result<VisionProposal, CommandError> {
        self.propose_calls.fetch_add(1, Ordering::SeqCst);
        Ok(VisionProposal {
            confidence: self.confidence,
            action: VisionAction::Click { x: 10.0, y: 20.0 },
        })
    }
}

#[derive(Default)]
struct RecordingProposals {
    inner: std::sync::Mutex<std::collections::HashMap<String, intent_engine::CachedProposal>>,
    record_calls: AtomicUsize,
}

impl intent_engine::ProposalLookup for RecordingProposals {
    fn proposal_for(&self, _page: &PageId, purpose: &str) -> Option<intent_engine::CachedProposal> {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(purpose.trim().to_lowercase().as_str())
            .cloned()
    }
    fn drop_proposal(&self, _page: &PageId, purpose: &str) {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(purpose.trim().to_lowercase().as_str());
    }
    fn record_proposals(
        &self,
        _page: &PageId,
        proposals: Vec<(String, intent_engine::CachedProposal)>,
    ) {
        self.record_calls.fetch_add(1, Ordering::SeqCst);
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        for (purpose, cached) in proposals {
            inner.insert(purpose.trim().to_lowercase(), cached);
        }
    }
}

fn text_field(name: &str, purpose: &str) -> types::CompleteFormField {
    types::CompleteFormField {
        name: name.into(),
        purpose: purpose.into(),
        hints: IntentHints::default(),
        value: types::FillValue::Text {
            text: format!("value-{name}"),
            clear_first: true,
        },
    }
}

#[tokio::test]
async fn complete_form_batches_one_screenshot_for_all_stuck_fields() {
    let propose_calls = Arc::new(AtomicUsize::new(0));
    let screenshot_calls = Arc::new(AtomicUsize::new(0));
    let assist = Arc::new(CountingVision {
        propose_calls: propose_calls.clone(),
        confidence: 0.9,
    });
    let proposals = Arc::new(RecordingProposals::default());
    // No candidates for any field: every fill gets stuck.
    let browser = CountingScreenshotBrowser {
        inner: FakeBrowser::default(),
        screenshot_calls: screenshot_calls.clone(),
    };
    let page_id = PageId::new();
    let intent = IntentCommand::CompleteForm(types::CompleteFormIntent {
        purpose: "sign up".into(),
        fields: vec![
            text_field("first", "First name"),
            text_field("last", "Last name"),
            text_field("city", "City"),
        ],
    });

    let outcome = IntentEngine::execute(
        &intent,
        &page_id,
        &browser,
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(assist),
            proposals: Some(proposals.clone()),
            defer_escalation: false,
            prompt_context: None,
        },
    )
    .await;

    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    assert_eq!(
        screenshot_calls.load(Ordering::SeqCst),
        1,
        "one screenshot for the whole form"
    );
    assert_eq!(
        propose_calls.load(Ordering::SeqCst),
        3,
        "one propose per stuck purpose"
    );
    let prefill_records = evidence
        .iter()
        .filter(|item| matches!(item, Evidence::IntentExecution { record } if record.resolution_path == IntentResolutionPath::VisionPrefill))
        .count();
    assert_eq!(prefill_records, 3, "every field resolved from the batch");
}

struct CountingScreenshotBrowser {
    inner: FakeBrowser,
    screenshot_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl IntentBrowser for CountingScreenshotBrowser {
    async fn collect_candidates(
        &self,
        page_id: &PageId,
        target: &TargetSpec,
    ) -> Result<Vec<dom_engine::Candidate>, CommandError> {
        self.inner.collect_candidates(page_id, target).await
    }
    async fn click(
        &self,
        page_id: &PageId,
        command: &ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.inner.click(page_id, command).await
    }
    async fn click_xy(
        &self,
        page_id: &PageId,
        x: f64,
        y: f64,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.inner.click_xy(page_id, x, y).await
    }
    async fn type_text(
        &self,
        page_id: &PageId,
        command: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.inner.type_text(page_id, command).await
    }
    async fn upload_files(
        &self,
        page_id: &PageId,
        command: &UploadFilesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.inner.upload_files(page_id, command).await
    }
    async fn wait_for(
        &self,
        page_id: &PageId,
        command: &WaitForCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.inner.wait_for(page_id, command).await
    }
    async fn capture_screenshot(
        &self,
        page_id: &PageId,
        command: &CaptureScreenshotCommand,
    ) -> Result<(Vec<u8>, Vec<Evidence>), CommandError> {
        self.screenshot_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.capture_screenshot(page_id, command).await
    }
}

#[tokio::test]
async fn provider_loss_during_batch_degrades_to_the_deterministic_path() {
    struct OfflineVision;
    #[async_trait]
    impl VisionAssist for OfflineVision {
        async fn propose(
            &self,
            _request: VisionProposeRequest,
        ) -> Result<VisionProposal, CommandError> {
            Err(CommandError {
                code: ErrorCode::VisionAssistFailed,
                message: "connection refused".into(),
                layer: types::ErrorLayer::Page,
                retryable: false,
            })
        }
    }
    let proposals = Arc::new(RecordingProposals::default());
    let browser = FakeBrowser::default();
    let page_id = PageId::new();
    let intent = IntentCommand::CompleteForm(types::CompleteFormIntent {
        purpose: "sign up".into(),
        fields: vec![text_field("first", "First name")],
    });

    let outcome = IntentEngine::execute(
        &intent,
        &page_id,
        &browser,
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(Arc::new(OfflineVision)),
            proposals: Some(proposals),
            defer_escalation: false,
            prompt_context: None,
        },
    )
    .await;

    // The batch records nothing; the retry escalates live, the provider is
    // offline, and the failure is the ordinary vision failure — the form
    // never panics, never hangs, and the stuck evidence is preserved.
    let IntentOutcome::Failed { error, evidence } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::VisionAssistFailed);
    assert!(
        evidence.iter().any(|item| matches!(item, Evidence::IntentExecution { record } if record.verification == "targetNotFound")),
        "stuck evidence lost during provider-loss degradation"
    );
}
