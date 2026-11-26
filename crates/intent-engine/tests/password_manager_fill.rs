use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use dom_engine::{Candidate, CandidateState};
use intent_engine::{IntentBrowser, IntentEngine, IntentOutcome, VisionContext};
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandError, ControlAction, ControlActionCommand,
    ControlActionEvidence, ErrorCode, Evidence, ExecutionRecord, FillIntent, FormControl,
    FormControlKind, FormControlOperation, FormControlState, FormControlValidity, FormSnapshot,
    IntentCommand, IntentHints, PageId, TargetSpec, TypeTextCommand, UploadFilesCommand,
    WaitForCommand,
};

/// Where the worker found the manager's own prompt frame on the page.
const PROMPT: (f64, f64) = (612.0, 40.0);

/// A login page with a password-manager extension: its prompt sits in the
/// extension's own frame, and only a click on it fills the password field.
/// The page may also open a dialog while the password field stays. A code
/// dialog changes the form fields, while an informational dialog does not.
struct ExtensionPage {
    prompt: Option<(f64, f64)>,
    filled: Mutex<bool>,
    dialog: Option<PromptDialog>,
    watch_error: Option<ErrorCode>,
}

#[derive(Clone, Copy)]
enum PromptDialog {
    Information,
    Code,
}

fn control(name: &str, control_kind: FormControlKind) -> FormControl {
    FormControl {
        id: name.into(),
        form_id: None,
        group_id: None,
        target: None,
        control_kind,
        accessible_name: Some(name.into()),
        label: None,
        description: None,
        placeholder: None,
        autocomplete: None,
        state: FormControlState::Empty,
        constraints: Default::default(),
        validity: FormControlValidity {
            will_validate: false,
            valid: true,
            flags: Vec::new(),
            message: None,
            described_by: Vec::new(),
        },
        options: Vec::new(),
        supported_operations: Vec::new(),
    }
}

fn candidate(id: &str, role: &str, name: &str) -> Candidate {
    Candidate {
        id: id.into(),
        css: Some(format!("#{id}")),
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

#[async_trait]
impl IntentBrowser for ExtensionPage {
    async fn collect_candidates(
        &self,
        _page_id: &PageId,
        _target: &TargetSpec,
    ) -> Result<Vec<Candidate>, CommandError> {
        let mut password = candidate("pass", "textbox", "Password");
        password.attributes = BTreeMap::from([("type".into(), "password".into())]);
        let mut census = vec![password];
        if *self.filled.lock().expect("fill state") {
            match self.dialog {
                Some(PromptDialog::Information) => {
                    census.push(candidate("information", "dialog", "About password fill"));
                }
                Some(PromptDialog::Code) => {
                    census.push(candidate("code", "dialog", "Enter the code"));
                }
                None => {}
            }
        }
        Ok(census)
    }

    async fn form_snapshot(&self, page_id: &PageId) -> Result<Option<FormSnapshot>, CommandError> {
        let mut controls = vec![control("Password", FormControlKind::Password)];
        if *self.filled.lock().expect("fill state") {
            match self.dialog {
                Some(PromptDialog::Code) => controls.push(control("Code", FormControlKind::Text)),
                None => controls.push(control("Show password", FormControlKind::Other)),
                Some(PromptDialog::Information) => {}
            }
        }
        Ok(Some(FormSnapshot {
            schema_version: types::FORM_SNAPSHOT_SCHEMA_VERSION,
            page_id: page_id.clone(),
            forms: Vec::new(),
            unowned_controls: controls,
            truncated: false,
            page_busy: false,
        }))
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
        if self.prompt == Some((x, y)) {
            *self.filled.lock().expect("fill state") = true;
        }
        Ok(Vec::new())
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

    async fn control_action(
        &self,
        _page_id: &PageId,
        command: &ControlActionCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let present = *self.filled.lock().expect("fill state");
        if let Some(code) = self.watch_error.filter(|_| present) {
            return Err(CommandError {
                code,
                message: "field watch failed".into(),
                layer: types::ErrorLayer::Driver,
                retryable: false,
            });
        }
        Ok(vec![Evidence::ControlAction {
            action: ControlActionEvidence {
                operation: FormControlOperation::PasswordManagerFill,
                target: command.target.clone(),
                state: FormControlState::Redacted { present },
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

    async fn password_manager_prompt(&self, _page_id: &PageId) -> Option<(f64, f64)> {
        self.prompt
    }
}

fn unsupported(op: &str) -> CommandError {
    CommandError {
        code: ErrorCode::Internal,
        message: format!("{op} not supported by this page"),
        layer: types::ErrorLayer::Page,
        retryable: false,
    }
}

fn password_manager_fill() -> IntentCommand {
    IntentCommand::Fill(FillIntent {
        purpose: "Sign in with the saved login".into(),
        hints: IntentHints {
            role: Some("textbox".into()),
            accessible_name: Some("Password".into()),
            ..IntentHints::default()
        },
        value: ControlAction::PasswordManagerFill,
    })
}

fn execution_record(evidence: &[Evidence]) -> &ExecutionRecord {
    evidence
        .iter()
        .rev()
        .find_map(|item| match item {
            Evidence::IntentExecution { record } => Some(record),
            _ => None,
        })
        .expect("fill execution record")
}

#[tokio::test]
async fn fill_only_with_a_new_show_password_button_is_reported_as_filled() {
    let page = ExtensionPage {
        prompt: Some(PROMPT),
        filled: Mutex::new(false),
        dialog: None,
        watch_error: None,
    };
    let outcome = IntentEngine::execute(
        &password_manager_fill(),
        &PageId::new(),
        &page,
        &VisionContext::default(),
    )
    .await;
    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("the extension prompt was not pressed: {outcome:?}");
    };
    assert!(*page.filled.lock().expect("fill state"));
    assert_eq!(
        execution_record(&evidence).verification,
        "passwordManagerFilled"
    );
}

#[tokio::test]
async fn a_manager_submit_that_opens_a_dialog_with_a_new_field_reports_both() {
    let page = ExtensionPage {
        prompt: Some(PROMPT),
        filled: Mutex::new(false),
        dialog: Some(PromptDialog::Code),
        watch_error: None,
    };
    let outcome = IntentEngine::execute(
        &password_manager_fill(),
        &PageId::new(),
        &page,
        &VisionContext::default(),
    )
    .await;
    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("the manager fill did not complete: {outcome:?}");
    };
    let record = execution_record(&evidence);
    assert_eq!(
        record.verification, "passwordManagerSubmitted",
        "{evidence:?}"
    );
    assert!(
        record.candidates.iter().any(|candidate| {
            candidate.role.as_deref() == Some("dialog")
                && candidate.name.as_deref() == Some("Enter the code")
        }),
        "{evidence:?}"
    );
    assert!(evidence
        .iter()
        .any(|item| matches!(item, Evidence::SubmitSettlement { .. })));
}

#[tokio::test]
async fn a_manager_submit_that_fails_the_field_read_reports_the_submit() {
    let page = ExtensionPage {
        prompt: Some(PROMPT),
        filled: Mutex::new(false),
        dialog: Some(PromptDialog::Code),
        watch_error: Some(ErrorCode::BrowserCommandFailed),
    };
    let outcome = IntentEngine::execute(
        &password_manager_fill(),
        &PageId::new(),
        &page,
        &VisionContext::default(),
    )
    .await;
    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("the manager's submit was reported as a failure: {outcome:?}");
    };
    assert_eq!(
        execution_record(&evidence).verification,
        "passwordManagerSubmitted",
        "{evidence:?}"
    );
}

#[tokio::test]
async fn a_dialog_alone_does_not_recover_a_failed_field_read() {
    let page = ExtensionPage {
        prompt: Some(PROMPT),
        filled: Mutex::new(false),
        dialog: Some(PromptDialog::Information),
        watch_error: Some(ErrorCode::BrowserCommandFailed),
    };
    let outcome = IntentEngine::execute(
        &password_manager_fill(),
        &PageId::new(),
        &page,
        &VisionContext::default(),
    )
    .await;
    let IntentOutcome::Failed { error, .. } = outcome else {
        panic!("a dialog alone was reported as a submit: {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::BrowserCommandFailed);
}

#[tokio::test]
async fn a_new_dialog_and_fields_do_not_override_a_field_validation_error() {
    let page = ExtensionPage {
        prompt: Some(PROMPT),
        filled: Mutex::new(false),
        dialog: Some(PromptDialog::Code),
        watch_error: Some(ErrorCode::IntentActionMismatch),
    };
    let outcome = IntentEngine::execute(
        &password_manager_fill(),
        &PageId::new(),
        &page,
        &VisionContext::default(),
    )
    .await;
    let IntentOutcome::Failed { error, .. } = outcome else {
        panic!("a field validation error was reported as a submit: {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::IntentActionMismatch);
}

#[tokio::test]
async fn without_a_located_prompt_the_fill_still_asks_for_vision() {
    let page = ExtensionPage {
        prompt: None,
        filled: Mutex::new(false),
        dialog: None,
        watch_error: None,
    };
    let outcome = IntentEngine::execute(
        &password_manager_fill(),
        &PageId::new(),
        &page,
        &VisionContext::default(),
    )
    .await;
    let IntentOutcome::Failed { error, .. } = outcome else {
        panic!("a fill with no prompt and no vision completed: {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::VisionAssistDenied);
    assert!(!*page.filled.lock().expect("fill state"));
}
