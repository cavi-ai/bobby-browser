use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use intent_engine::{
    IntentBrowser, IntentEngine, IntentOutcome, VisionAction, VisionAssist, VisionContext,
    VisionCorpus, VisionProposal, VisionProposeRequest, VISION_CONFIDENCE_FLOOR,
};
use observability::{OperationalMetrics, ProviderMode};
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandError, ErrorCode, Evidence, FillIntent,
    FillValue, IntentCommand, IntentHints, IntentResolutionPath, LocateIntent, PageId, TargetSpec,
    TypeTextCommand, UploadFilesCommand, WaitForCommand,
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
    click_targets: Arc<std::sync::Mutex<Vec<Option<types::TargetSpec>>>>,
    type_text_calls: Arc<std::sync::Mutex<Vec<TypeTextCommand>>>,
    type_text_evidence: Vec<Evidence>,
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
        command: &ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.click_targets
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(command.target.clone());
        Ok(vec![Evidence::Configuration {
            name: "visionClick".into(),
            value: "ok".into(),
        }])
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
        command: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.type_text_calls
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(command.clone());
        Ok(self.type_text_evidence.clone())
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

struct MetricVision {
    metrics: OperationalMetrics,
    proposal: VisionProposal,
}

#[async_trait]
impl VisionAssist for MetricVision {
    async fn propose(
        &self,
        _request: VisionProposeRequest,
    ) -> Result<VisionProposal, CommandError> {
        Ok(self.proposal.clone())
    }

    fn operational_metrics(&self) -> Option<(OperationalMetrics, ProviderMode)> {
        Some((self.metrics.clone(), ProviderMode::DirectLocal))
    }
}

#[tokio::test]
async fn vision_metrics_distinguish_rejected_from_verified_actions() {
    let rejected_metrics = OperationalMetrics::default();
    let rejected = IntentEngine::execute(
        &locate(),
        &PageId::new(),
        &FakeBrowser::default(),
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(Arc::new(MetricVision {
                metrics: rejected_metrics.clone(),
                proposal: click_proposal(VISION_CONFIDENCE_FLOOR - 0.01),
            })),
            proposals: None,
            defer_escalation: false,
            prompt_context: None,
            corpus: None,
        },
    )
    .await;
    assert!(matches!(rejected, IntentOutcome::Failed { .. }));
    let rejected_snapshot = rejected_metrics.snapshot();
    assert_eq!(rejected_snapshot.vision.rejected, 1);
    assert_eq!(rejected_snapshot.vision.accepted, 0);
    assert_eq!(rejected_snapshot.vision.confidence.below_acceptance, 1);

    let accepted_metrics = OperationalMetrics::default();
    let accepted = IntentEngine::execute(
        &locate(),
        &PageId::new(),
        &FakeBrowser::default(),
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(Arc::new(MetricVision {
                metrics: accepted_metrics.clone(),
                proposal: click_proposal(0.95),
            })),
            proposals: None,
            defer_escalation: false,
            prompt_context: None,
            corpus: None,
        },
    )
    .await;
    assert!(matches!(accepted, IntentOutcome::Completed { .. }));
    let accepted_snapshot = accepted_metrics.snapshot();
    assert_eq!(accepted_snapshot.vision.accepted, 1);
    assert_eq!(accepted_snapshot.vision.rejected, 0);
    assert_eq!(accepted_snapshot.vision.provider_direct_local, 1);
    assert_eq!(accepted_snapshot.verification.accepted, 1);
}

struct RecordingVision {
    proposal: VisionProposal,
    request_debug: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl VisionAssist for RecordingVision {
    async fn propose(&self, request: VisionProposeRequest) -> Result<VisionProposal, CommandError> {
        self.request_debug
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(format!("{request:?}"));
        Ok(self.proposal.clone())
    }
}

fn form_candidate(id: &str, role: &str, name: &str) -> dom_engine::Candidate {
    dom_engine::Candidate {
        id: id.into(),
        css: Some(format!("#{id}")),
        test_id: None,
        role: Some(role.into()),
        name: Some(name.into()),
        label: None,
        text: name.into(),
        attributes: BTreeMap::new(),
        state: dom_engine::CandidateState {
            attached: true,
            visible: true,
            enabled: true,
        },
        frame_path: Vec::new(),
    }
}

fn fill(purpose: &str, role: &str, value: FillValue) -> IntentCommand {
    IntentCommand::Fill(FillIntent {
        purpose: purpose.into(),
        hints: IntentHints {
            role: Some(role.into()),
            ..IntentHints::default()
        },
        value,
    })
}

#[tokio::test]
async fn type_into_candidate_uses_runtime_text_without_disclosing_it_to_the_provider() {
    let request_debug = Arc::new(std::sync::Mutex::new(Vec::new()));
    let type_text_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let click_xy_calls = Arc::new(AtomicUsize::new(0));
    let assist = Arc::new(RecordingVision {
        proposal: VisionProposal {
            confidence: 0.95,
            action: VisionAction::TypeIntoCandidate { index: 1 },
        },
        request_debug: request_debug.clone(),
    });
    let browser = FakeBrowser {
        candidates: vec![
            form_candidate("primary-email", "textbox", "Contact field"),
            form_candidate("work-email", "textbox", "Contact field"),
        ],
        click_xy_calls: click_xy_calls.clone(),
        type_text_calls: type_text_calls.clone(),
        type_text_evidence: vec![Evidence::Element {
            selector: "#work-email".into(),
            text: Some("runtime secret".into()),
        }],
        screenshot_png: b"png".to_vec(),
        ..FakeBrowser::default()
    };
    let dir = tempfile::tempdir().expect("temp corpus directory");

    let outcome = IntentEngine::execute(
        &fill(
            "Contact field",
            "textbox",
            FillValue::Text {
                text: "runtime secret".into(),
                clear_first: true,
            },
        ),
        &PageId::new(),
        &browser,
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(assist),
            proposals: None,
            defer_escalation: false,
            prompt_context: None,
            corpus: Some(VisionCorpus::new(dir.path()).expect("vision corpus")),
        },
    )
    .await;

    assert!(
        matches!(outcome, IntentOutcome::Completed { .. }),
        "expected completed fill, got {outcome:?}"
    );
    let requests = request_debug.lock().unwrap_or_else(|p| p.into_inner());
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("Contact field"));
    assert!(!requests[0].contains("runtime secret"));
    let calls = type_text_calls.lock().unwrap_or_else(|p| p.into_inner());
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]
            .target
            .as_ref()
            .and_then(|target| target.role.as_deref()),
        Some("textbox")
    );
    assert_eq!(
        calls[0]
            .target
            .as_ref()
            .and_then(|target| target.accessible_name.as_deref()),
        Some("Contact field")
    );
    assert_eq!(
        calls[0].target.as_ref().and_then(|target| target.ordinal),
        Some(1),
        "candidate index 1 must become ordinal 1 among duplicate semantic identities"
    );
    assert_eq!(calls[0].value, "runtime secret");
    assert!(calls[0].clear_first);
    assert_eq!(click_xy_calls.load(Ordering::SeqCst), 0);
    let corpus = std::fs::read_to_string(dir.path().join("vision-corpus.jsonl")).unwrap();
    assert!(!corpus.contains("runtime secret"));
    let record: serde_json::Value = serde_json::from_str(corpus.trim()).unwrap();
    assert_eq!(record["targetIndex"], 1);
}

#[tokio::test]
async fn type_into_candidate_verification_failure_redacts_runtime_text_from_corpus_and_error() {
    const SECRET: &str = "phase17-corpus-secret-7e6e2e77";
    let assist = Arc::new(FakeVision {
        called: Arc::new(AtomicBool::new(false)),
        proposal: VisionProposal {
            confidence: 0.95,
            action: VisionAction::TypeIntoCandidate { index: 0 },
        },
    });
    let browser = FakeBrowser {
        candidates: vec![
            form_candidate("primary-email", "textbox", "Contact field"),
            form_candidate("work-email", "textbox", "Contact field"),
        ],
        type_text_evidence: vec![Evidence::Element {
            selector: "#primary-email".into(),
            text: Some("not-the-runtime-value".into()),
        }],
        screenshot_png: b"png".to_vec(),
        ..FakeBrowser::default()
    };
    let dir = tempfile::tempdir().expect("temp corpus directory");
    let corpus = VisionCorpus::new(dir.path()).expect("vision corpus");

    let outcome = IntentEngine::execute(
        &fill(
            "Contact field",
            "textbox",
            FillValue::Text {
                text: SECRET.into(),
                clear_first: true,
            },
        ),
        &PageId::new(),
        &browser,
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(assist),
            proposals: None,
            defer_escalation: false,
            prompt_context: None,
            corpus: Some(corpus),
        },
    )
    .await;

    let IntentOutcome::Failed { error, evidence } = outcome else {
        panic!("expected verification failure, got {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::VisionAssistFailed);
    assert!(!error.message.contains(SECRET));
    assert!(!format!("{evidence:?}").contains(SECRET));

    let corpus = std::fs::read_to_string(dir.path().join("vision-corpus.jsonl"))
        .expect("recorded corpus entry");
    assert!(!corpus.contains(SECRET));
    assert!(corpus.contains("typeIntoCandidate verification failed"));
    let record: serde_json::Value = serde_json::from_str(corpus.trim()).unwrap();
    assert_eq!(record["success"], false);
    assert!(record.get("targetIndex").is_none());
}

#[tokio::test]
async fn type_into_candidate_out_of_range_fails_closed_without_mutation() {
    let type_text_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let assist = Arc::new(FakeVision {
        called: Arc::new(AtomicBool::new(false)),
        proposal: VisionProposal {
            confidence: 0.95,
            action: VisionAction::TypeIntoCandidate { index: 7 },
        },
    });
    let browser = FakeBrowser {
        candidates: vec![
            form_candidate("primary-email", "textbox", "Contact field"),
            form_candidate("work-email", "textbox", "Contact field"),
        ],
        type_text_calls: type_text_calls.clone(),
        screenshot_png: b"png".to_vec(),
        ..FakeBrowser::default()
    };

    let outcome = IntentEngine::execute(
        &fill(
            "Contact field",
            "textbox",
            FillValue::Text {
                text: "runtime secret".into(),
                clear_first: true,
            },
        ),
        &PageId::new(),
        &browser,
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(assist),
            proposals: None,
            defer_escalation: false,
            prompt_context: None,
            corpus: None,
        },
    )
    .await;

    assert!(matches!(
        outcome,
        IntentOutcome::Failed { error, .. } if error.code == ErrorCode::VisionAssistFailed
    ));
    assert!(type_text_calls
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .is_empty());
}

#[tokio::test]
async fn type_into_candidate_rejects_non_text_fill_without_mutation() {
    let type_text_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let assist = Arc::new(FakeVision {
        called: Arc::new(AtomicBool::new(false)),
        proposal: VisionProposal {
            confidence: 0.95,
            action: VisionAction::TypeIntoCandidate { index: 0 },
        },
    });
    let browser = FakeBrowser {
        candidates: vec![
            form_candidate("home-state", "combobox", "State field"),
            form_candidate("work-state", "combobox", "State field"),
        ],
        type_text_calls: type_text_calls.clone(),
        screenshot_png: b"png".to_vec(),
        ..FakeBrowser::default()
    };

    let outcome = IntentEngine::execute(
        &fill(
            "State field",
            "combobox",
            FillValue::Select {
                option: "CA".into(),
            },
        ),
        &PageId::new(),
        &browser,
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(assist),
            proposals: None,
            defer_escalation: false,
            prompt_context: None,
            corpus: None,
        },
    )
    .await;

    assert!(matches!(
        outcome,
        IntentOutcome::Failed { error, .. } if error.code == ErrorCode::VisionAssistFailed
    ));
    assert!(type_text_calls
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .is_empty());
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
            corpus: None,
        },
    )
    .await;

    let IntentOutcome::Failed { error, evidence } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::VisionAssistDenied);
    // The message leads with the deterministic stuck reason and names the
    // closed gate: an agent that never asked for vision can repair the
    // target instead of reading a policy wall.
    assert!(
        error.message.starts_with("no candidate matched") || error.message.starts_with("target"),
        "{}",
        error.message
    );
    assert!(
        error
            .message
            .contains("vision assist is off for this session (executionPolicy.visionAssist)"),
        "{}",
        error.message
    );
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
async fn click_candidate_proposal_clicks_the_referenced_element() {
    fn candidate(name: &str) -> dom_engine::Candidate {
        dom_engine::Candidate {
            id: format!("btn-{name}"),
            css: Some(format!("[data-name=\"{name}\"]")),
            test_id: None,
            role: Some("button".into()),
            name: Some(name.into()),
            label: None,
            text: name.into(),
            attributes: Default::default(),
            state: dom_engine::CandidateState {
                attached: true,
                visible: true,
                enabled: true,
            },
            frame_path: Vec::new(),
        }
    }

    // Ask for a link; the page only has buttons, so resolution fails with
    // candidates present and the escalation carries them.
    let locate_link = IntentCommand::Locate(LocateIntent {
        purpose: "Continue to checkout".into(),
        hints: IntentHints {
            role: Some("link".into()),
            ..IntentHints::default()
        },
    });

    let called = Arc::new(AtomicBool::new(false));
    let click_xy_calls = Arc::new(AtomicUsize::new(0));
    let click_targets: Arc<std::sync::Mutex<Vec<Option<types::TargetSpec>>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let assist = Arc::new(FakeVision {
        called: called.clone(),
        proposal: VisionProposal {
            confidence: 0.95,
            action: VisionAction::ClickCandidate { index: 0 },
        },
    });
    let browser = FakeBrowser {
        candidates: vec![candidate("Continue"), candidate("Cancel")],
        click_xy_calls: click_xy_calls.clone(),
        click_targets: click_targets.clone(),
        screenshot_png: b"png".to_vec(),
        ..FakeBrowser::default()
    };
    let page_id = PageId::new();

    let outcome = IntentEngine::execute(
        &locate_link,
        &page_id,
        &browser,
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(assist),
            proposals: None,
            defer_escalation: false,
            prompt_context: None,
            corpus: None,
        },
    )
    .await;

    let IntentOutcome::Completed { .. } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    assert!(called.load(Ordering::SeqCst), "propose must be called");
    assert_eq!(
        click_xy_calls.load(Ordering::SeqCst),
        0,
        "clickCandidate must not fall to pixel clicking"
    );
    let targets = click_targets.lock().unwrap_or_else(|p| p.into_inner());
    assert_eq!(targets.len(), 1, "exactly one DOM click expected");
    let target = targets[0].as_ref().expect("click must carry a target spec");
    assert_eq!(target.role.as_deref(), Some("button"));
    assert_eq!(target.accessible_name.as_deref(), Some("Continue"));
}

#[tokio::test]
async fn click_candidate_index_outside_the_prompt_list_fails_closed() {
    fn candidate(name: &str) -> dom_engine::Candidate {
        dom_engine::Candidate {
            id: format!("btn-{name}"),
            css: Some(format!("[data-name=\"{name}\"]")),
            test_id: None,
            role: Some("button".into()),
            name: Some(name.into()),
            label: None,
            text: name.into(),
            attributes: Default::default(),
            state: dom_engine::CandidateState {
                attached: true,
                visible: true,
                enabled: true,
            },
            frame_path: Vec::new(),
        }
    }

    let locate_link = IntentCommand::Locate(LocateIntent {
        purpose: "Continue to checkout".into(),
        hints: IntentHints {
            role: Some("link".into()),
            ..IntentHints::default()
        },
    });

    let called = Arc::new(AtomicBool::new(false));
    let assist = Arc::new(FakeVision {
        called: called.clone(),
        proposal: VisionProposal {
            confidence: 0.95,
            action: VisionAction::ClickCandidate { index: 7 },
        },
    });
    let browser = FakeBrowser {
        candidates: vec![candidate("Cancel")],
        screenshot_png: b"png".to_vec(),
        ..FakeBrowser::default()
    };

    let outcome = IntentEngine::execute(
        &locate_link,
        &PageId::new(),
        &browser,
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(assist),
            proposals: None,
            defer_escalation: false,
            prompt_context: None,
            corpus: None,
        },
    )
    .await;

    let IntentOutcome::Failed { error, .. } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::VisionAssistFailed);
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
            corpus: None,
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
            corpus: None,
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
            corpus: None,
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
            corpus: None,
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
            corpus: None,
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
            corpus: None,
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
            corpus: None,
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
                corpus: None,
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
            corpus: None,
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
    metrics: OperationalMetrics,
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

    fn operational_metrics(&self) -> Option<(OperationalMetrics, ProviderMode)> {
        Some((self.metrics.clone(), ProviderMode::DirectLocal))
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
    let metrics = OperationalMetrics::default();
    let assist = Arc::new(CountingVision {
        propose_calls: propose_calls.clone(),
        confidence: 0.9,
        metrics: metrics.clone(),
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
            corpus: None,
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
    assert_eq!(metrics.snapshot().vision.accepted, 3);
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
            corpus: None,
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
