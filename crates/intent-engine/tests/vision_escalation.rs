use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use context_store::{
    ContextStore, ControlContext, FormContext, IntentStats, PageContext as StoredPageContext,
    RecordSource, SiteContext,
};
use intent_engine::{
    collect_vision_training_data, instrument_vision_assist, IntentBrowser, IntentEngine,
    IntentOutcome, VisionAction, VisionAssist, VisionContext, VisionCorpus, VisionPromptContext,
    VisionProposal, VisionProposeRequest, VISION_CONFIDENCE_FLOOR,
};
use observability::{OperationalMetrics, ProviderMode};
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandError, ControlAction, ControlActionCommand,
    ControlActionEvidence, ErrorCode, Evidence, FillIntent, FormControlOperation, FormControlState,
    FormControlValidity, IntentCommand, IntentHints, IntentResolutionPath, LocateIntent, PageId,
    TargetSpec, TypeTextCommand, UploadFilesCommand, WaitForCommand,
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
    upload_files_calls: Arc<std::sync::Mutex<Vec<UploadFilesCommand>>>,
    control_action_calls: Arc<std::sync::Mutex<Vec<ControlActionCommand>>>,
    type_text_evidence: Vec<Evidence>,
    upload_files_evidence: Vec<Evidence>,
    upload_files_error: Option<CommandError>,
    rejected_control_target: Option<String>,
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
        command: &UploadFilesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.upload_files_calls
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(command.clone());
        if let Some(error) = &self.upload_files_error {
            return Err(error.clone());
        }
        Ok(self.upload_files_evidence.clone())
    }

    async fn control_action(
        &self,
        _page_id: &PageId,
        command: &ControlActionCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.control_action_calls
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(command.clone());
        if self.rejected_control_target.as_deref() == Some(command.target.accessible_name.as_str())
        {
            return Err(CommandError {
                code: ErrorCode::TargetNotFound,
                message: "target changed".into(),
                layer: types::ErrorLayer::Page,
                retryable: false,
            });
        }
        let (operation, state) = match &command.action {
            ControlAction::SelectOne { value } => (
                FormControlOperation::SelectOne,
                FormControlState::Selection {
                    values: vec![value.clone()],
                },
            ),
            ControlAction::SelectMany { values } => (
                FormControlOperation::SelectMany,
                FormControlState::Selection {
                    values: values.clone(),
                },
            ),
            ControlAction::SetChecked { checked } => (
                FormControlOperation::SetChecked,
                FormControlState::Checked { checked: *checked },
            ),
            ControlAction::Clear => (FormControlOperation::Clear, FormControlState::Empty),
            _ => return Err(unsupported("control_action")),
        };
        Ok(vec![Evidence::ControlAction {
            action: ControlActionEvidence {
                operation,
                target: command.target.clone(),
                state,
                validity: FormControlValidity {
                    will_validate: true,
                    valid: true,
                    flags: Vec::new(),
                    message: None,
                    described_by: Vec::new(),
                },
                node_replaced: false,
                revealed_controls: Vec::new(),
            },
        }])
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

    async fn capture_sanitized_screenshot(
        &self,
        _page_id: &PageId,
    ) -> Result<Vec<u8>, CommandError> {
        Ok(self.screenshot_png.clone())
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
            context_store: None,
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
            context_store: None,
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

struct CorpusFrameVision {
    frame: Arc<std::sync::Mutex<Option<Vec<u8>>>>,
}

#[async_trait]
impl VisionAssist for CorpusFrameVision {
    async fn propose(&self, request: VisionProposeRequest) -> Result<VisionProposal, CommandError> {
        *self.frame.lock().unwrap_or_else(|p| p.into_inner()) = request.corpus_screenshot_png;
        Ok(click_proposal(0.91))
    }
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
        tag: None,
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

fn file_candidate(id: &str, name: &str) -> dom_engine::Candidate {
    let mut candidate = form_candidate(id, "button", name);
    candidate.attributes.insert("type".into(), "file".into());
    candidate
}

fn fill(purpose: &str, role: &str, value: ControlAction) -> IntentCommand {
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
async fn fill_not_found_escalates_with_a_ranked_window() {
    // A fill that matches nothing must escalate with the page's plausible
    // fields in the window. The fill path used to escalate with an EMPTY
    // window — the model was asked to pick from nothing, correctly
    // abstained, and the records were §4i poison.
    let request_debug = Arc::new(std::sync::Mutex::new(Vec::new()));
    let assist = Arc::new(RecordingVision {
        proposal: VisionProposal {
            confidence: 0.95,
            action: VisionAction::TypeIntoCandidate { index: 0 },
        },
        request_debug: request_debug.clone(),
    });
    let browser = FakeBrowser {
        candidates: vec![
            form_candidate("brand", "link", "Northstar Ops"),
            form_candidate("search-field", "searchbox", "Search customers"),
        ],
        type_text_evidence: vec![Evidence::Element {
            selector: "#search-field".into(),
            text: Some("Atlas".into()),
        }],
        screenshot_png: b"png".to_vec(),
        ..FakeBrowser::default()
    };

    let outcome = IntentEngine::execute(
        &IntentCommand::Fill(FillIntent {
            purpose: "Put 'Atlas' in the lookup box".into(), // no name match
            hints: IntentHints::default(),
            value: ControlAction::SetText {
                value: "Atlas".into(),
                clear_first: true,
            },
        }),
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
            context_store: None,
        },
    )
    .await;

    assert!(
        matches!(outcome, IntentOutcome::Completed { .. }),
        "expected completed fill via escalation, got {outcome:?}"
    );
    let requests = request_debug.lock().unwrap_or_else(|p| p.into_inner());
    let request = requests.last().expect("vision was not consulted");
    assert!(
        request.contains("Search customers"),
        "fill escalation window must carry the plausible field: {request}"
    );
    assert!(
        !request.contains("runtime secret"),
        "payload must never reach the provider"
    );
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
            ControlAction::SetText {
                value: "runtime secret".into(),
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
            context_store: None,
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
async fn type_into_candidate_uploads_runtime_files_without_disclosing_paths() {
    const PRIVATE_PATH: &str = "/private/runtime-resume-secret.pdf";
    let request_debug = Arc::new(std::sync::Mutex::new(Vec::new()));
    let upload_files_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let assist = Arc::new(RecordingVision {
        proposal: VisionProposal {
            confidence: 0.95,
            action: VisionAction::TypeIntoCandidate { index: 1 },
        },
        request_debug: request_debug.clone(),
    });
    let browser = FakeBrowser {
        candidates: vec![
            file_candidate("primary-resume", "Resume"),
            file_candidate("secondary-resume", "Resume"),
        ],
        upload_files_calls: upload_files_calls.clone(),
        upload_files_evidence: vec![Evidence::Upload {
            selector: String::new(),
            paths: vec![PRIVATE_PATH.into()],
        }],
        screenshot_png: b"png".to_vec(),
        ..FakeBrowser::default()
    };
    let dir = tempfile::tempdir().expect("temp corpus directory");

    let outcome = IntentEngine::execute(
        &fill(
            "Resume",
            "button",
            ControlAction::SetFiles {
                paths: vec![PRIVATE_PATH.into()],
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
            context_store: None,
        },
    )
    .await;

    assert!(
        matches!(outcome, IntentOutcome::Completed { .. }),
        "expected completed upload, got {outcome:?}"
    );
    let requests = request_debug.lock().unwrap_or_else(|p| p.into_inner());
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].contains(PRIVATE_PATH));
    let calls = upload_files_calls.lock().unwrap_or_else(|p| p.into_inner());
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].paths, [PRIVATE_PATH]);
    let target = calls[0].target.as_ref().expect("candidate target");
    assert_eq!(target.role.as_deref(), Some("button"));
    assert_eq!(target.accessible_name.as_deref(), Some("Resume"));
    assert_eq!(target.ordinal, Some(1));
    let corpus = std::fs::read_to_string(dir.path().join("vision-corpus.jsonl")).unwrap();
    assert!(!corpus.contains(PRIVATE_PATH));
}

#[tokio::test]
async fn type_into_candidate_redacts_runtime_file_paths_when_upload_fails() {
    const PRIVATE_PATH: &str = "/private/runtime-resume-secret.pdf";
    let upload_files_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let assist = Arc::new(FakeVision {
        called: Arc::new(AtomicBool::new(false)),
        proposal: VisionProposal {
            confidence: 0.95,
            action: VisionAction::TypeIntoCandidate { index: 0 },
        },
    });
    let browser = FakeBrowser {
        candidates: vec![
            file_candidate("primary-resume", "Resume"),
            file_candidate("secondary-resume", "Resume"),
        ],
        upload_files_calls: upload_files_calls.clone(),
        upload_files_error: Some(CommandError {
            code: ErrorCode::PolicyDenied,
            message: format!("upload path is outside configured roots: {PRIVATE_PATH}"),
            layer: types::ErrorLayer::Interface,
            retryable: false,
        }),
        screenshot_png: b"png".to_vec(),
        ..FakeBrowser::default()
    };
    let dir = tempfile::tempdir().expect("temp corpus directory");

    let outcome = IntentEngine::execute(
        &fill(
            "Resume",
            "button",
            ControlAction::SetFiles {
                paths: vec![PRIVATE_PATH.into()],
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
            context_store: None,
        },
    )
    .await;

    assert_eq!(
        upload_files_calls
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len(),
        1
    );
    let IntentOutcome::Failed { error, evidence } = outcome else {
        panic!("expected upload failure, got {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::VisionAssistFailed);
    assert!(!error.message.contains(PRIVATE_PATH));
    assert!(!format!("{evidence:?}").contains(PRIVATE_PATH));
    let corpus = std::fs::read_to_string(dir.path().join("vision-corpus.jsonl")).unwrap();
    assert!(!corpus.contains(PRIVATE_PATH));
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
            ControlAction::SetText {
                value: SECRET.into(),
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
            context_store: None,
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
    let record: serde_json::Value = serde_json::from_str(corpus.trim()).unwrap();
    assert_eq!(record["success"], false);
    assert!(record.get("targetIndex").is_none());
    assert!(record.get("errorMessage").is_none());
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
            ControlAction::SetText {
                value: "runtime secret".into(),
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
            context_store: None,
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
async fn type_into_candidate_applies_runtime_control_actions_without_disclosing_values() {
    let cases = [
        (
            "combobox",
            ControlAction::SelectOne {
                value: "runtime-ca".into(),
            },
            "runtime-ca",
        ),
        (
            "listbox",
            ControlAction::SelectMany {
                values: vec!["runtime-red".into(), "runtime-blue".into()],
            },
            "runtime-red",
        ),
        ("checkbox", ControlAction::SetChecked { checked: true }, ""),
        ("textbox", ControlAction::Clear, ""),
    ];

    for (role, action, runtime_value) in cases {
        let request_debug = Arc::new(std::sync::Mutex::new(Vec::new()));
        let control_action_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let dir = tempfile::tempdir().expect("temp corpus directory");
        let assist = Arc::new(RecordingVision {
            proposal: VisionProposal {
                confidence: 0.95,
                action: VisionAction::TypeIntoCandidate { index: 1 },
            },
            request_debug: request_debug.clone(),
        });
        let browser = FakeBrowser {
            candidates: vec![
                form_candidate("primary-control", role, "Account field"),
                form_candidate("secondary-control", role, "Account field"),
            ],
            control_action_calls: control_action_calls.clone(),
            screenshot_png: b"png".to_vec(),
            ..FakeBrowser::default()
        };

        let outcome = IntentEngine::execute(
            &fill("Account field", role, action.clone()),
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
                context_store: None,
            },
        )
        .await;

        assert!(
            matches!(outcome, IntentOutcome::Completed { .. }),
            "expected completed {action:?}, got {outcome:?}"
        );
        let calls = control_action_calls
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].action, action);
        assert_eq!(calls[0].target.ordinal, Some(1));
        if !runtime_value.is_empty() {
            let requests = request_debug.lock().unwrap_or_else(|p| p.into_inner());
            assert!(!requests[0].contains(runtime_value));
            let corpus = std::fs::read_to_string(dir.path().join("vision-corpus.jsonl")).unwrap();
            assert!(!corpus.contains(runtime_value));
        }
    }
}

#[tokio::test]
async fn type_into_candidate_rejects_incompatible_control_kind_without_mutation() {
    let control_action_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let assist = Arc::new(FakeVision {
        called: Arc::new(AtomicBool::new(false)),
        proposal: VisionProposal {
            confidence: 0.95,
            action: VisionAction::TypeIntoCandidate { index: 0 },
        },
    });
    let browser = FakeBrowser {
        candidates: vec![
            form_candidate("primary-name", "textbox", "Account field"),
            form_candidate("secondary-name", "textbox", "Account field"),
        ],
        control_action_calls: control_action_calls.clone(),
        screenshot_png: b"png".to_vec(),
        ..FakeBrowser::default()
    };

    let outcome = IntentEngine::execute(
        &fill(
            "Account field",
            "textbox",
            ControlAction::SelectOne {
                value: "runtime-ca".into(),
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
            context_store: None,
        },
    )
    .await;

    assert!(matches!(
        outcome,
        IntentOutcome::Failed { error, .. } if error.code == ErrorCode::VisionAssistFailed
    ));
    assert!(control_action_calls
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .is_empty());
}

#[tokio::test]
async fn stuck_without_vision_gates_returns_the_stuck_code() {
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
            context_store: None,
        },
    )
    .await;

    let IntentOutcome::Failed { error, evidence } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    // The stuck code leads (`targetNotFound` here), not `visionAssistDenied`:
    // an agent's repair logic keys on `code`, so the code itself must say
    // what is actually wrong with the target.
    assert_eq!(error.code, ErrorCode::TargetNotFound);
    // The message keeps both sentences: the deterministic stuck reason and
    // the closed gate, so an agent that never asked for vision can repair
    // the target instead of reading the code as a policy wall.
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
async fn stuck_with_no_vision_provider_returns_the_stuck_code() {
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
            session_ok: true,
            capability_ok: true,
            assist: None,
            proposals: None,
            defer_escalation: false,
            prompt_context: None,
            corpus: None,
            context_store: None,
        },
    )
    .await;

    let IntentOutcome::Failed { error, .. } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::TargetNotFound);
    assert!(
        error.message.starts_with("no candidate matched") || error.message.starts_with("target"),
        "{}",
        error.message
    );
    assert!(
        error
            .message
            .contains("no vision fallback ran because no vision provider is configured"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn click_candidate_proposal_clicks_the_referenced_element() {
    fn candidate(name: &str) -> dom_engine::Candidate {
        dom_engine::Candidate {
            id: format!("btn-{name}"),
            css: Some(format!("[data-name=\"{name}\"]")),
            tag: None,
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
            context_store: None,
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
            tag: None,
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
            context_store: None,
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
            context_store: None,
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
async fn proxy_collection_receives_a_masked_frame_without_local_corpus() {
    let frame = Arc::new(std::sync::Mutex::new(None));
    let assist = collect_vision_training_data(Arc::new(CorpusFrameVision {
        frame: frame.clone(),
    }));
    let browser = FakeBrowser {
        screenshot_png: b"masked-frame".to_vec(),
        ..FakeBrowser::default()
    };

    let outcome = IntentEngine::execute(
        &locate(),
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
            context_store: None,
        },
    )
    .await;

    assert!(matches!(outcome, IntentOutcome::Completed { .. }));
    assert_eq!(
        frame.lock().unwrap_or_else(|p| p.into_inner()).as_deref(),
        Some(b"masked-frame".as_slice())
    );
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
            context_store: None,
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
            context_store: None,
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
            context_store: None,
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
        ErrorCode::TargetNotFound,
        "a missing capability was not reported under the stuck kind's own code"
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
            context_store: None,
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
    assert_eq!(error.code, ErrorCode::TargetNotFound);
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
        action: intent_engine::CachedProposalAction::Click { x: 12.0, y: 34.0 },
        confidence,
    }
}

fn cached_type_candidate(name: &str) -> intent_engine::CachedProposal {
    intent_engine::CachedProposal {
        action: intent_engine::CachedProposalAction::TypeIntoCandidate {
            candidates: vec![intent_engine::VisionPromptCandidate {
                role: "checkbox".into(),
                name: name.into(),
                ordinal: None,
            }],
            index: 0,
        },
        confidence: 0.95,
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
            context_store: None,
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
            context_store: None,
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
async fn escalation_window_excludes_landmark_rows() {
    // The production census includes landmark/structural rows (main, nav,
    // region…) since the scoped a11y snapshot widened the role map. The
    // adapter's training windows are actionable-role only; a landmark row
    // shifts the prompt off distribution (measured: one `main` row flips
    // the adapter to abstain). The window must gate them out.
    let request_debug = Arc::new(std::sync::Mutex::new(Vec::new()));
    let assist = Arc::new(RecordingVision {
        proposal: click_proposal(0.91),
        request_debug: request_debug.clone(),
    });
    let browser = FakeBrowser {
        screenshot_png: b"png".to_vec(),
        candidates: vec![
            form_candidate("app-main", "main", "Name  Continue Resume"),
            form_candidate("name-field", "textbox", "Name"),
            form_candidate("go", "button", "Continue"),
            form_candidate("back", "button", "Resume"),
        ],
        click_xy_calls: Arc::new(AtomicUsize::new(0)),
        ..FakeBrowser::default()
    };
    let page_id = PageId::new();
    let intent = IntentCommand::Locate(LocateIntent {
        purpose: "Proceed to checkout".into(), // matches nothing → escalate
        hints: IntentHints::default(),
    });

    let outcome = IntentEngine::execute(
        &intent,
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
            context_store: None,
        },
    )
    .await;

    let IntentOutcome::Completed { .. } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    let requests = request_debug.lock().unwrap_or_else(|p| p.into_inner());
    let request = requests.last().expect("vision was not consulted");
    assert!(
        !request.contains("Name  Continue Resume"),
        "landmark row leaked into the vision window: {request}"
    );
    assert!(
        request.contains("Continue") && request.contains("Resume"),
        "actionable rows missing from the vision window: {request}"
    );
}

#[tokio::test]
async fn near_miss_window_is_purpose_ranked() {
    // DOM order on a real page puts sidebar chrome first; an unranked top-5
    // window truncates the page's actionable content out of the prompt and
    // the model correctly abstains on a target it cannot see. The window
    // must float rows that share a token with the purpose.
    let request_debug = Arc::new(std::sync::Mutex::new(Vec::new()));
    let assist = Arc::new(RecordingVision {
        proposal: click_proposal(0.91),
        request_debug: request_debug.clone(),
    });
    let browser = FakeBrowser {
        screenshot_png: b"png".to_vec(),
        candidates: vec![
            form_candidate("brand", "link", "Northstar Ops"),
            form_candidate("nav-overview", "link", "Overview"),
            form_candidate("nav-customers", "link", "Customers"),
            form_candidate("nav-onboarding", "link", "Onboarding"),
            form_candidate("nav-documents", "link", "Documents"),
            form_candidate("nav-integrations", "link", "Integrations"),
            form_candidate("nav-reports", "link", "Reports"),
            form_candidate("search-field", "searchbox", "Search customers"),
            form_candidate("search-go", "button", "Search"),
        ],
        click_xy_calls: Arc::new(AtomicUsize::new(0)),
        ..FakeBrowser::default()
    };
    let page_id = PageId::new();
    let intent = IntentCommand::Locate(LocateIntent {
        purpose: "Push the button to search".into(),
        hints: IntentHints::default(),
    });

    let outcome = IntentEngine::execute(
        &intent,
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
            context_store: None,
        },
    )
    .await;

    let IntentOutcome::Completed { .. } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    let requests = request_debug.lock().unwrap_or_else(|p| p.into_inner());
    let request = requests.last().expect("vision was not consulted");
    assert!(
        request.contains("Search customers"),
        "the purpose-relevant row must enter the window: {request}"
    );
    assert!(
        !request.contains("Onboarding"),
        "sidebar chrome must not fill the window: {request}"
    );
}

#[tokio::test]
async fn verified_page_context_ranks_the_provider_candidate_window() {
    let temp = tempfile::tempdir().unwrap();
    let (store, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    store
        .upsert_site(
            "https://example.test",
            SiteContext {
                pages: BTreeMap::from([(
                    "/checkout".into(),
                    StoredPageContext {
                        forms: BTreeMap::from([(
                            "page".into(),
                            FormContext {
                                controls: vec![ControlContext {
                                    role: "button".into(),
                                    accessible_name: "Review order".into(),
                                    ordinal: None,
                                    form_membership: "page".into(),
                                    intents: BTreeMap::from([(
                                        "locate".into(),
                                        IntentStats {
                                            success_count: 3,
                                            failure_count: 0,
                                            last_verified_day: Some(20_000),
                                            source: Some(RecordSource::Observed),
                                        },
                                    )]),
                                }],
                            },
                        )]),
                    },
                )]),
                ..SiteContext::default()
            },
        )
        .await;

    let request_debug = Arc::new(std::sync::Mutex::new(Vec::new()));
    let metrics = OperationalMetrics::default();
    let assist = instrument_vision_assist(
        Arc::new(RecordingVision {
            proposal: VisionProposal {
                confidence: 0.95,
                action: VisionAction::ClickCandidate { index: 0 },
            },
            request_debug: request_debug.clone(),
        }),
        metrics.clone(),
    );
    let click_targets = Arc::new(std::sync::Mutex::new(Vec::new()));
    let browser = FakeBrowser {
        candidates: vec![
            form_candidate("one", "button", "Account"),
            form_candidate("two", "button", "Settings"),
            form_candidate("three", "button", "Support"),
            form_candidate("four", "button", "History"),
            form_candidate("five", "button", "Cancel"),
            form_candidate("review", "button", "Review order"),
        ],
        click_targets: click_targets.clone(),
        screenshot_png: b"png".to_vec(),
        ..FakeBrowser::default()
    };

    let outcome = IntentEngine::execute(
        &IntentCommand::Locate(LocateIntent {
            purpose: "Finish checkout".into(),
            hints: IntentHints::default(),
        }),
        &PageId::new(),
        &browser,
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(assist),
            proposals: None,
            defer_escalation: false,
            prompt_context: Some(VisionPromptContext {
                url: Some("https://example.test/checkout?step=2".into()),
                ..VisionPromptContext::default()
            }),
            corpus: None,
            context_store: Some(Arc::new(store)),
        },
    )
    .await;

    assert!(matches!(outcome, IntentOutcome::Completed { .. }));
    let targets = click_targets.lock().unwrap_or_else(|p| p.into_inner());
    assert_eq!(
        targets[0]
            .as_ref()
            .and_then(|target| target.accessible_name.as_deref()),
        Some("Review order")
    );
    assert_eq!(metrics.snapshot().context.hit, 1);
    let ranked = metrics.snapshot().context_ranked_vision;
    assert_eq!(ranked.attempted, 1);
    assert_eq!(ranked.source_observed, 1);
    assert_eq!(ranked.hit, 1);
    assert_eq!(ranked.provider_escalations, 1);
    assert_eq!(ranked.provider_direct_local, 1);
    assert_eq!(ranked.confidence.high, 1);
    assert_eq!(ranked.verification_accepted, 1);
    assert_eq!(ranked.verification_rejected, 0);
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
                context_store: None,
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
            context_store: None,
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

#[tokio::test]
async fn candidate_drift_drops_the_cached_identity_and_recovers_live() {
    let called = Arc::new(AtomicBool::new(false));
    let assist = Arc::new(FakeVision {
        called: called.clone(),
        proposal: VisionProposal {
            confidence: 0.95,
            action: VisionAction::TypeIntoCandidate { index: 0 },
        },
    });
    let proposals = Arc::new(FakeProposals {
        hits: [(
            "notification contact".to_string(),
            cached_type_candidate("Previous contact"),
        )]
        .into_iter()
        .collect(),
        consulted: Arc::new(AtomicBool::new(false)),
        dropped: Arc::new(AtomicUsize::new(0)),
    });
    let dropped = proposals.dropped.clone();
    let browser = FakeBrowser {
        candidates: vec![
            form_candidate("current", "checkbox", "Current contact"),
            form_candidate("alternate", "checkbox", "Alternate contact"),
        ],
        rejected_control_target: Some("Previous contact".into()),
        screenshot_png: b"png".to_vec(),
        ..FakeBrowser::default()
    };

    let outcome = IntentEngine::execute(
        &fill(
            "Notification contact",
            "checkbox",
            ControlAction::SetChecked { checked: true },
        ),
        &PageId::new(),
        &browser,
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(assist),
            proposals: Some(proposals),
            defer_escalation: false,
            prompt_context: None,
            corpus: None,
            context_store: None,
        },
    )
    .await;

    assert!(matches!(outcome, IntentOutcome::Completed { .. }));
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(called.load(Ordering::SeqCst));
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

struct ProactiveCandidateVision {
    propose_calls: Arc<AtomicUsize>,
    control_action_calls: Arc<std::sync::Mutex<Vec<ControlActionCommand>>>,
    expected_first_candidate: &'static str,
}

#[async_trait]
impl VisionAssist for ProactiveCandidateVision {
    async fn propose(&self, request: VisionProposeRequest) -> Result<VisionProposal, CommandError> {
        assert!(
            self.control_action_calls
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_empty(),
            "prefill ran after the form had already started mutating"
        );
        let candidates = request
            .context
            .as_ref()
            .map(|context| context.candidates.as_slice())
            .unwrap_or_default();
        assert!(
            !candidates.is_empty(),
            "proactive prefill did not send a candidate-grounded window"
        );
        assert_eq!(
            candidates[0].name, self.expected_first_candidate,
            "verified retained context did not rank the proactive window"
        );
        self.propose_calls.fetch_add(1, Ordering::SeqCst);
        Ok(VisionProposal {
            confidence: 0.95,
            action: VisionAction::TypeIntoCandidate { index: 0 },
        })
    }
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
            action: VisionAction::TypeIntoCandidate { index: 0 },
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
        value: types::ControlAction::SetText {
            value: format!("value-{name}"),
            clear_first: true,
        },
        revealed_by: None,
    }
}

fn checked_field(
    name: &str,
    purpose: &str,
    accessible_name: Option<&str>,
) -> types::CompleteFormField {
    types::CompleteFormField {
        name: name.into(),
        purpose: purpose.into(),
        hints: IntentHints {
            role: Some("checkbox".into()),
            accessible_name: accessible_name.map(str::to_owned),
            ..IntentHints::default()
        },
        value: types::ControlAction::SetChecked { checked: true },
        revealed_by: None,
    }
}

#[tokio::test]
async fn complete_form_prefills_ambiguous_fields_before_the_first_action() {
    let temp = tempfile::tempdir().unwrap();
    let (store, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    store
        .upsert_site(
            "https://example.test",
            SiteContext {
                pages: BTreeMap::from([(
                    "/settings".into(),
                    StoredPageContext {
                        forms: BTreeMap::from([(
                            "page".into(),
                            FormContext {
                                controls: vec![ControlContext {
                                    role: "checkbox".into(),
                                    accessible_name: "Backup contact".into(),
                                    ordinal: None,
                                    form_membership: "page".into(),
                                    intents: BTreeMap::from([(
                                        "fill".into(),
                                        IntentStats {
                                            success_count: 3,
                                            failure_count: 0,
                                            last_verified_day: Some(20_000),
                                            source: Some(RecordSource::Observed),
                                        },
                                    )]),
                                }],
                            },
                        )]),
                    },
                )]),
                ..SiteContext::default()
            },
        )
        .await;
    let propose_calls = Arc::new(AtomicUsize::new(0));
    let control_action_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let assist = Arc::new(ProactiveCandidateVision {
        propose_calls: propose_calls.clone(),
        control_action_calls: control_action_calls.clone(),
        expected_first_candidate: "Backup contact",
    });
    let browser = FakeBrowser {
        candidates: vec![
            form_candidate("terms", "checkbox", "Accept terms"),
            form_candidate("primary", "checkbox", "Primary contact"),
            form_candidate("backup", "checkbox", "Backup contact"),
        ],
        control_action_calls: control_action_calls.clone(),
        screenshot_png: b"png".to_vec(),
        ..FakeBrowser::default()
    };
    let intent = IntentCommand::CompleteForm(types::CompleteFormIntent {
        purpose: "configure notifications".into(),
        fields: vec![
            checked_field("terms", "Accept terms", Some("Accept terms")),
            checked_field("contact", "Choose a contact", None),
        ],
    });

    let outcome = IntentEngine::execute(
        &intent,
        &PageId::new(),
        &browser,
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(assist),
            proposals: Some(Arc::new(RecordingProposals::default())),
            defer_escalation: false,
            prompt_context: Some(VisionPromptContext {
                url: Some("https://example.test/settings".into()),
                ..VisionPromptContext::default()
            }),
            corpus: None,
            context_store: Some(Arc::new(store)),
        },
    )
    .await;

    assert!(
        matches!(outcome, IntentOutcome::Completed { .. }),
        "expected proactive candidate prefill to complete the form, got {outcome:?}"
    );
    assert_eq!(propose_calls.load(Ordering::SeqCst), 1);
    let actions = control_action_calls
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    assert_eq!(actions.len(), 2, "both runtime-owned actions must execute");
    assert_eq!(actions[0].target.accessible_name, "Accept terms");
}

struct ConcurrentVision {
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
}

struct ActiveVisionCall(Arc<AtomicUsize>);

impl Drop for ActiveVisionCall {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct PendingVision {
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
}

#[async_trait]
impl VisionAssist for PendingVision {
    async fn propose(
        &self,
        _request: VisionProposeRequest,
    ) -> Result<VisionProposal, CommandError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        let _call = ActiveVisionCall(self.active.clone());
        std::future::pending::<()>().await;
        unreachable!()
    }
}

#[async_trait]
impl VisionAssist for ConcurrentVision {
    async fn propose(
        &self,
        _request: VisionProposeRequest,
    ) -> Result<VisionProposal, CommandError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(VisionProposal {
            confidence: 0.95,
            action: VisionAction::TypeIntoCandidate { index: 0 },
        })
    }
}

#[tokio::test]
async fn proactive_prefill_limits_provider_concurrency_to_four() {
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let browser = FakeBrowser {
        candidates: vec![
            form_candidate("primary", "checkbox", "Primary contact"),
            form_candidate("backup", "checkbox", "Backup contact"),
        ],
        screenshot_png: b"png".to_vec(),
        ..FakeBrowser::default()
    };
    let fields = (0..6)
        .map(|index| checked_field(&format!("field-{index}"), &format!("choice-{index}"), None))
        .collect();

    let outcome = IntentEngine::execute(
        &IntentCommand::CompleteForm(types::CompleteFormIntent {
            purpose: "configure notifications".into(),
            fields,
        }),
        &PageId::new(),
        &browser,
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(Arc::new(ConcurrentVision {
                active: active.clone(),
                max_active: max_active.clone(),
            })),
            proposals: Some(Arc::new(RecordingProposals::default())),
            defer_escalation: false,
            prompt_context: None,
            corpus: None,
            context_store: None,
        },
    )
    .await;

    assert!(matches!(outcome, IntentOutcome::Completed { .. }));
    assert_eq!(max_active.load(Ordering::SeqCst), 4);
    assert_eq!(active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancelling_proactive_prefill_cancels_in_flight_provider_calls() {
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let task_active = active.clone();
    let task_max_active = max_active.clone();
    let task = tokio::spawn(async move {
        let browser = FakeBrowser {
            candidates: vec![
                form_candidate("primary", "checkbox", "Primary contact"),
                form_candidate("backup", "checkbox", "Backup contact"),
            ],
            screenshot_png: b"png".to_vec(),
            ..FakeBrowser::default()
        };
        let fields = (0..6)
            .map(|index| checked_field(&format!("field-{index}"), &format!("choice-{index}"), None))
            .collect();
        IntentEngine::execute(
            &IntentCommand::CompleteForm(types::CompleteFormIntent {
                purpose: "configure notifications".into(),
                fields,
            }),
            &PageId::new(),
            &browser,
            &VisionContext {
                session_ok: true,
                capability_ok: true,
                assist: Some(Arc::new(PendingVision {
                    active: task_active,
                    max_active: task_max_active,
                })),
                proposals: Some(Arc::new(RecordingProposals::default())),
                defer_escalation: false,
                prompt_context: None,
                corpus: None,
                context_store: None,
            },
        )
        .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while max_active.load(Ordering::SeqCst) < 4 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("provider calls did not start");
    task.abort();
    let _ = task.await;

    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(max_active.load(Ordering::SeqCst), 4);
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
    let browser = CountingScreenshotBrowser {
        inner: FakeBrowser {
            candidates: vec![
                form_candidate("primary", "checkbox", "Primary value"),
                form_candidate("alternate", "checkbox", "Alternate value"),
            ],
            screenshot_png: b"png".to_vec(),
            ..FakeBrowser::default()
        },
        screenshot_calls: screenshot_calls.clone(),
    };
    let page_id = PageId::new();
    let intent = IntentCommand::CompleteForm(types::CompleteFormIntent {
        purpose: "sign up".into(),
        fields: vec![
            checked_field("first", "First choice", None),
            checked_field("last", "Last choice", None),
            checked_field("city", "City choice", None),
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
            context_store: None,
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

#[tokio::test]
async fn deterministic_forms_do_not_call_the_prefill_provider_or_capture_a_screenshot() {
    let called = Arc::new(AtomicBool::new(false));
    let screenshot_calls = Arc::new(AtomicUsize::new(0));
    let browser = CountingScreenshotBrowser {
        inner: FakeBrowser {
            candidates: vec![form_candidate("terms", "checkbox", "Accept terms")],
            ..FakeBrowser::default()
        },
        screenshot_calls: screenshot_calls.clone(),
    };

    let outcome = IntentEngine::execute(
        &IntentCommand::CompleteForm(types::CompleteFormIntent {
            purpose: "accept terms".into(),
            fields: vec![checked_field("terms", "Accept terms", Some("Accept terms"))],
        }),
        &PageId::new(),
        &browser,
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(Arc::new(FakeVision {
                called: called.clone(),
                proposal: VisionProposal {
                    confidence: 0.95,
                    action: VisionAction::TypeIntoCandidate { index: 0 },
                },
            })),
            proposals: Some(Arc::new(RecordingProposals::default())),
            defer_escalation: false,
            prompt_context: None,
            corpus: None,
            context_store: None,
        },
    )
    .await;

    assert!(matches!(outcome, IntentOutcome::Completed { .. }));
    assert!(!called.load(Ordering::SeqCst));
    assert_eq!(screenshot_calls.load(Ordering::SeqCst), 0);
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
    async fn control_action(
        &self,
        page_id: &PageId,
        command: &ControlActionCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.inner.control_action(page_id, command).await
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
            context_store: None,
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
