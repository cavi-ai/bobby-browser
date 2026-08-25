use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dom_engine::{Candidate, CandidateState};
use intent_engine::{compatible, IntentBrowser, IntentEngine, IntentOutcome, VisionContext};
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandError, CompleteFormField, CompleteFormIntent,
    ControlAction, ControlActionCommand, ControlActionEvidence, ErrorCode, Evidence, FillIntent,
    FormControlOperation, FormControlState, FormControlValidity, IntentCommand, IntentHints,
    IntentResolutionPath, PageId, TargetSpec, TypeTextCommand, UploadFilesCommand, WaitForCommand,
};

#[tokio::test]
async fn complete_form_fills_fields_in_order_without_submitting() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let browser = FakeBrowser {
        candidates: Arc::new(vec![textbox("Full name"), textbox("Email address")]),
        calls: Arc::clone(&calls),
        type_text_evidence: vec![Evidence::Element {
            selector: String::new(),
            text: Some("Ada".into()),
        }],
        upload_evidence: vec![],
    };
    let field = |name: &str, label: &str| CompleteFormField {
        name: name.into(),
        purpose: format!("fill {label}"),
        hints: IntentHints {
            role: Some("textbox".into()),
            near_text: Some(types::TextMatch::Exact(label.into())),
            ..Default::default()
        },
        value: ControlAction::SetText {
            value: "Ada".into(),
            clear_first: true,
        },
    };
    let outcome = IntentEngine::execute(
        &IntentCommand::CompleteForm(CompleteFormIntent {
            purpose: "complete application".into(),
            fields: vec![field("name", "Full name"), field("email", "Email address")],
        }),
        &PageId::new(),
        &browser,
        &VisionContext::default(),
    )
    .await;
    assert!(matches!(outcome, IntentOutcome::Completed { .. }));
    assert_eq!(calls.lock().unwrap().type_text.len(), 2);
}

#[tokio::test]
async fn complete_form_ignores_incompatible_label_when_control_has_the_same_name() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let browser = FakeBrowser {
        candidates: Arc::new(vec![
            candidate("full-name-label", None, "Full name", BTreeMap::new()),
            textbox("Full name"),
        ]),
        calls: Arc::clone(&calls),
        type_text_evidence: vec![Evidence::Element {
            selector: String::new(),
            text: Some("Ada Lovelace".into()),
        }],
        upload_evidence: vec![],
    };
    let outcome = IntentEngine::execute(
        &IntentCommand::CompleteForm(CompleteFormIntent {
            purpose: "complete customer onboarding".into(),
            fields: vec![CompleteFormField {
                name: "Full name".into(),
                purpose: "customer full name".into(),
                hints: IntentHints::default(),
                value: ControlAction::SetText {
                    value: "Ada Lovelace".into(),
                    clear_first: true,
                },
            }],
        }),
        &PageId::new(),
        &browser,
        &VisionContext::default(),
    )
    .await;

    assert!(matches!(outcome, IntentOutcome::Completed { .. }));
    assert_eq!(calls.lock().unwrap().type_text.len(), 1);
}

#[derive(Default)]
struct CallLog {
    control_action: Vec<ControlActionCommand>,
    type_text: Vec<TypeTextCommand>,
    upload_files: Vec<UploadFilesCommand>,
}

struct FakeBrowser {
    candidates: Arc<Vec<Candidate>>,
    calls: Arc<Mutex<CallLog>>,
    type_text_evidence: Vec<Evidence>,
    upload_evidence: Vec<Evidence>,
}

#[async_trait]
impl IntentBrowser for FakeBrowser {
    async fn collect_candidates(
        &self,
        _page_id: &PageId,
        _target: &TargetSpec,
    ) -> Result<Vec<Candidate>, CommandError> {
        Ok((*self.candidates).clone())
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
        command: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.calls
            .lock()
            .expect("call log")
            .type_text
            .push(command.clone());
        Ok(self.type_text_evidence.clone())
    }

    async fn upload_files(
        &self,
        _page_id: &PageId,
        command: &UploadFilesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.calls
            .lock()
            .expect("call log")
            .upload_files
            .push(command.clone());
        Ok(self.upload_evidence.clone())
    }

    async fn control_action(
        &self,
        _page_id: &PageId,
        command: &ControlActionCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.calls
            .lock()
            .expect("call log")
            .control_action
            .push(command.clone());
        let (operation, state) = match &command.action {
            ControlAction::SelectOne { value } => (
                FormControlOperation::SelectOne,
                FormControlState::Selection {
                    values: vec![value.clone()],
                },
            ),
            ControlAction::SetChecked { checked } => (
                FormControlOperation::SetChecked,
                FormControlState::Checked { checked: *checked },
            ),
            ControlAction::SelectMany { values } => (
                FormControlOperation::SelectMany,
                FormControlState::Selection {
                    values: values.clone(),
                },
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
        Err(unsupported("capture_screenshot"))
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

fn candidate(
    id: &str,
    role: Option<&str>,
    name: &str,
    attributes: BTreeMap<String, String>,
) -> Candidate {
    Candidate {
        id: id.into(),
        css: Some(format!("#{id}")),
        test_id: None,
        role: role.map(str::to_owned),
        name: Some(name.into()),
        label: None,
        text: name.into(),
        attributes,
        state: CandidateState {
            attached: true,
            visible: true,
            enabled: true,
        },
        frame_path: Vec::new(),
    }
}

fn textbox(name: &str) -> Candidate {
    candidate(name, Some("textbox"), name, BTreeMap::new())
}

fn file_input(name: &str) -> Candidate {
    let mut attributes = BTreeMap::new();
    attributes.insert("type".into(), "file".into());
    // Worker scan maps INPUT (incl. type=file) to role=textbox with type attribute.
    candidate(name, Some("textbox"), name, attributes)
}

fn combobox(name: &str) -> Candidate {
    candidate(name, Some("combobox"), name, BTreeMap::new())
}

fn listbox(name: &str) -> Candidate {
    candidate(name, Some("listbox"), name, BTreeMap::new())
}

fn fill(purpose: &str, role: Option<&str>, value: ControlAction) -> IntentCommand {
    IntentCommand::Fill(FillIntent {
        purpose: purpose.into(),
        hints: IntentHints {
            role: role.map(str::to_owned),
            ..IntentHints::default()
        },
        value,
    })
}

#[test]
fn compatible_matches_worker_candidate_signals() {
    assert!(compatible(
        &ControlAction::SetText {
            value: "Ada".into(),
            clear_first: true,
        },
        &textbox("Email")
    ));
    assert!(!compatible(
        &ControlAction::SetFiles {
            paths: vec!["/tmp/a.txt".into()],
        },
        &textbox("Email")
    ));
    assert!(compatible(
        &ControlAction::SetFiles {
            paths: vec!["/tmp/a.txt".into()],
        },
        &file_input("Resume")
    ));
    assert!(!compatible(
        &ControlAction::SetText {
            value: "Ada".into(),
            clear_first: true,
        },
        &file_input("Resume")
    ));
    assert!(compatible(
        &ControlAction::SelectOne { value: "CA".into() },
        &combobox("State")
    ));
    assert!(!compatible(
        &ControlAction::SelectOne { value: "CA".into() },
        &textbox("Email")
    ));
    assert!(compatible(
        &ControlAction::SetChecked { checked: true },
        &candidate("updates", Some("checkbox"), "Updates", BTreeMap::new())
    ));
    assert!(compatible(
        &ControlAction::SetChecked { checked: true },
        &candidate(
            "professional",
            Some("radio"),
            "Professional",
            BTreeMap::new()
        )
    ));
    assert!(compatible(
        &ControlAction::SelectMany {
            values: vec!["ham".into()]
        },
        &listbox("Toppings")
    ));
    assert!(!compatible(
        &ControlAction::SelectMany {
            values: vec!["ham".into()]
        },
        &combobox("State")
    ));
    assert!(compatible(&ControlAction::Clear, &textbox("Email")));
    assert!(compatible(&ControlAction::Activate, &textbox("Email")));
}

#[tokio::test]
async fn fill_text_types_and_verifies() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let browser = FakeBrowser {
        candidates: Arc::new(vec![textbox("Email")]),
        calls: Arc::clone(&calls),
        type_text_evidence: vec![Evidence::Element {
            selector: "#Email".into(),
            text: Some("Ada".into()),
        }],
        upload_evidence: Vec::new(),
    };
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &fill(
            "Email",
            Some("textbox"),
            ControlAction::SetText {
                value: "Ada".into(),
                clear_first: true,
            },
        ),
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
        assert_eq!(log.type_text.len(), 1);
        assert_eq!(log.type_text[0].value, "Ada");
        assert!(log.type_text[0].clear_first);
        assert!(log.upload_files.is_empty());
    }
    let record = evidence.iter().find_map(|item| match item {
        Evidence::IntentExecution { record } => Some(record),
        _ => None,
    });
    let record = record.expect("IntentExecution evidence");
    assert_eq!(record.intent_kind, "fill");
    assert_eq!(record.purpose.as_deref(), Some("Email"));
    assert_eq!(record.resolution_path, IntentResolutionPath::Deterministic);
    assert_eq!(record.verification, "filled");
}

#[tokio::test]
async fn fill_select_dispatches_a_typed_control_action_instead_of_text_input() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let browser = FakeBrowser {
        candidates: Arc::new(vec![combobox("State")]),
        calls: Arc::clone(&calls),
        type_text_evidence: vec![Evidence::Element {
            selector: "#State".into(),
            text: Some("CA".into()),
        }],
        upload_evidence: Vec::new(),
    };
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &fill(
            "State",
            Some("combobox"),
            ControlAction::SelectOne { value: "CA".into() },
        ),
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
        assert!(log.type_text.is_empty());
        assert_eq!(log.control_action.len(), 1);
    }
    assert!(evidence.iter().any(|item| matches!(
        item,
        Evidence::IntentExecution { record } if record.verification == "filled"
    )));
}

#[tokio::test]
async fn fill_checked_dispatches_a_typed_control_action_instead_of_text_input() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let browser = FakeBrowser {
        candidates: Arc::new(vec![candidate(
            "updates",
            Some("checkbox"),
            "Updates",
            BTreeMap::new(),
        )]),
        calls: Arc::clone(&calls),
        type_text_evidence: vec![Evidence::Element {
            selector: "#updates".into(),
            text: Some("true".into()),
        }],
        upload_evidence: Vec::new(),
    };

    let outcome = IntentEngine::execute(
        &fill(
            "Updates",
            Some("checkbox"),
            ControlAction::SetChecked { checked: true },
        ),
        &PageId::new(),
        &browser,
        &VisionContext::default(),
    )
    .await;

    assert!(matches!(outcome, IntentOutcome::Completed { .. }));
    let log = calls.lock().expect("call log");
    assert!(log.type_text.is_empty());
    assert!(matches!(
        log.control_action.as_slice(),
        [ControlActionCommand {
            action: ControlAction::SetChecked { checked: true },
            ..
        }]
    ));
}

#[tokio::test]
async fn fill_fails_when_the_worker_returns_no_postcondition_evidence() {
    let browser = FakeBrowser {
        candidates: Arc::new(vec![textbox("Email")]),
        calls: Arc::new(Mutex::new(CallLog::default())),
        type_text_evidence: Vec::new(),
        upload_evidence: Vec::new(),
    };
    let outcome = IntentEngine::execute(
        &fill(
            "Email",
            Some("textbox"),
            ControlAction::SetText {
                value: "ada@example.test".into(),
                clear_first: true,
            },
        ),
        &PageId::new(),
        &browser,
        &VisionContext::default(),
    )
    .await;

    assert!(matches!(
        outcome,
        IntentOutcome::Failed { error, .. }
            if error.code == ErrorCode::VerificationFailed
                && error.message.contains("missing typed-value evidence")
    ));
}

#[tokio::test]
async fn fill_rejects_browser_invalid_control_even_when_value_matches() {
    let browser = FakeBrowser {
        candidates: Arc::new(vec![textbox("Postal code")]),
        calls: Arc::new(Mutex::new(CallLog::default())),
        type_text_evidence: vec![
            Evidence::Element {
                selector: "#postal-code".into(),
                text: Some("12".into()),
            },
            Evidence::Configuration {
                name: "formControlValid".into(),
                value: "false".into(),
            },
            Evidence::Configuration {
                name: "formControlValidationMessage".into(),
                value: "Please match the requested format.".into(),
            },
        ],
        upload_evidence: Vec::new(),
    };

    let outcome = IntentEngine::execute(
        &fill(
            "Postal code",
            Some("textbox"),
            ControlAction::SetText {
                value: "12".into(),
                clear_first: true,
            },
        ),
        &PageId::new(),
        &browser,
        &VisionContext::default(),
    )
    .await;

    assert!(matches!(
        outcome,
        IntentOutcome::Failed { error, evidence }
            if error.code == ErrorCode::VerificationFailed
                && error.message.contains("Please match the requested format")
                && evidence.iter().any(|item| matches!(
                    item,
                    Evidence::Configuration { name, value }
                        if name == "formControlValid" && value == "false"
                ))
    ));
}

#[tokio::test]
async fn fill_files_uploads_and_verifies() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let browser = FakeBrowser {
        candidates: Arc::new(vec![file_input("Resume")]),
        calls: Arc::clone(&calls),
        type_text_evidence: Vec::new(),
        upload_evidence: vec![Evidence::Upload {
            selector: "#Resume".into(),
            paths: vec!["/tmp/resume.txt".into()],
        }],
    };
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &fill(
            "Resume",
            Some("textbox"),
            ControlAction::SetFiles {
                paths: vec!["/tmp/resume.txt".into()],
            },
        ),
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
        assert_eq!(log.upload_files.len(), 1);
        assert_eq!(
            log.upload_files[0].paths,
            vec!["/tmp/resume.txt".to_owned()]
        );
        assert!(log.type_text.is_empty());
    }
    assert!(evidence.iter().any(|item| matches!(
        item,
        Evidence::IntentExecution { record } if record.verification == "filled"
    )));
}

#[tokio::test]
async fn fill_files_on_textbox_is_action_mismatch() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let browser = FakeBrowser {
        candidates: Arc::new(vec![textbox("Email")]),
        calls: Arc::clone(&calls),
        type_text_evidence: Vec::new(),
        upload_evidence: Vec::new(),
    };
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &fill(
            "Email",
            Some("textbox"),
            ControlAction::SetFiles {
                paths: vec!["/tmp/resume.txt".into()],
            },
        ),
        &page_id,
        &browser,
        &VisionContext::default(),
    )
    .await;

    let IntentOutcome::Failed { error, evidence } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::IntentActionMismatch);
    {
        let log = calls.lock().expect("call log");
        assert!(log.type_text.is_empty());
        assert!(log.upload_files.is_empty());
    }
    let record = evidence.iter().find_map(|item| match item {
        Evidence::IntentExecution { record } => Some(record),
        _ => None,
    });
    let record = record.expect("IntentExecution on mismatch");
    assert_eq!(record.verification, "actionMismatch");
    assert_eq!(record.resolution_path, IntentResolutionPath::Deterministic);
}

#[tokio::test]
async fn fill_select_many_dispatches_a_typed_control_action() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let browser = FakeBrowser {
        candidates: Arc::new(vec![listbox("Toppings")]),
        calls: Arc::clone(&calls),
        type_text_evidence: Vec::new(),
        upload_evidence: Vec::new(),
    };
    let outcome = IntentEngine::execute(
        &fill(
            "Toppings",
            Some("listbox"),
            ControlAction::SelectMany {
                values: vec!["ham".into(), "olives".into()],
            },
        ),
        &PageId::new(),
        &browser,
        &VisionContext::default(),
    )
    .await;

    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    {
        let log = calls.lock().expect("call log");
        assert!(log.type_text.is_empty());
        assert!(matches!(
            log.control_action.as_slice(),
            [ControlActionCommand {
                action: ControlAction::SelectMany { values },
                ..
            }] if values == &["ham".to_owned(), "olives".to_owned()]
        ));
    }
    assert!(evidence.iter().any(|item| matches!(
        item,
        Evidence::IntentExecution { record } if record.verification == "filled"
    )));
}

#[tokio::test]
async fn fill_select_many_on_a_single_select_is_action_mismatch() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let browser = FakeBrowser {
        candidates: Arc::new(vec![combobox("State")]),
        calls: Arc::clone(&calls),
        type_text_evidence: Vec::new(),
        upload_evidence: Vec::new(),
    };
    let outcome = IntentEngine::execute(
        &fill(
            "State",
            Some("combobox"),
            ControlAction::SelectMany {
                values: vec!["CA".into()],
            },
        ),
        &PageId::new(),
        &browser,
        &VisionContext::default(),
    )
    .await;

    let IntentOutcome::Failed { error, .. } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::IntentActionMismatch);
    assert!(calls.lock().expect("call log").control_action.is_empty());
}

#[tokio::test]
async fn fill_clear_dispatches_a_typed_control_action() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let browser = FakeBrowser {
        candidates: Arc::new(vec![textbox("Email")]),
        calls: Arc::clone(&calls),
        type_text_evidence: Vec::new(),
        upload_evidence: Vec::new(),
    };
    let outcome = IntentEngine::execute(
        &fill("Email", Some("textbox"), ControlAction::Clear),
        &PageId::new(),
        &browser,
        &VisionContext::default(),
    )
    .await;

    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    {
        let log = calls.lock().expect("call log");
        assert!(log.type_text.is_empty());
        assert!(matches!(
            log.control_action.as_slice(),
            [ControlActionCommand {
                action: ControlAction::Clear,
                ..
            }]
        ));
    }
    assert!(evidence.iter().any(|item| matches!(
        item,
        Evidence::IntentExecution { record } if record.verification == "filled"
    )));
}

/// `activate` deserializes fine into a fill's `ControlAction` value (the
/// wire vocabulary is shared with control_action) but is never valid for
/// fill/complete_form: activating a control is not a value to fill. It must
/// fail clearly, naming `control_action` as the right tool, instead of
/// silently no-op'ing or dispatching a click.
#[tokio::test]
async fn fill_rejects_activate_and_names_control_action_as_the_right_tool() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let browser = FakeBrowser {
        candidates: Arc::new(vec![textbox("Email")]),
        calls: Arc::clone(&calls),
        type_text_evidence: Vec::new(),
        upload_evidence: Vec::new(),
    };
    let outcome = IntentEngine::execute(
        &fill("Email", Some("textbox"), ControlAction::Activate),
        &PageId::new(),
        &browser,
        &VisionContext::default(),
    )
    .await;

    let IntentOutcome::Failed { error, .. } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(!error.retryable);
    assert!(
        error.message.contains("control_action"),
        "unexpected message {:?}",
        error.message
    );
    let log = calls.lock().expect("call log");
    assert!(log.type_text.is_empty());
    assert!(log.control_action.is_empty());
    assert!(log.upload_files.is_empty());
}
