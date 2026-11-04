use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use dom_engine::{Candidate, CandidateState};
use intent_engine::{IntentBrowser, IntentEngine, IntentOutcome, VisionContext};
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandError, ControlAction, ControlActionCommand,
    ControlActionEvidence, ErrorCode, Evidence, FillIntent, FormControlOperation, FormControlState,
    FormControlValidity, IntentCommand, IntentHints, PageId, TargetSpec, TypeTextCommand,
    UploadFilesCommand, WaitForCommand,
};

/// Where the worker found the manager's own prompt frame on the page.
const PROMPT: (f64, f64) = (612.0, 40.0);

/// A login page with a password-manager extension: its prompt sits in the
/// extension's own frame, and only a click on it fills the password field.
struct ExtensionPage {
    prompt: Option<(f64, f64)>,
    filled: Mutex<bool>,
}

#[async_trait]
impl IntentBrowser for ExtensionPage {
    async fn collect_candidates(
        &self,
        _page_id: &PageId,
        _target: &TargetSpec,
    ) -> Result<Vec<Candidate>, CommandError> {
        Ok(vec![Candidate {
            id: "pass".into(),
            css: Some("#pass".into()),
            tag: None,
            test_id: None,
            role: Some("textbox".into()),
            name: Some("Password".into()),
            label: None,
            text: "Password".into(),
            attributes: BTreeMap::from([("type".into(), "password".into())]),
            state: CandidateState {
                attached: true,
                visible: true,
                enabled: true,
            },
            frame_path: Vec::new(),
        }])
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

#[tokio::test]
async fn a_located_manager_prompt_is_pressed_without_vision() {
    let page = ExtensionPage {
        prompt: Some(PROMPT),
        filled: Mutex::new(false),
    };
    let outcome = IntentEngine::execute(
        &password_manager_fill(),
        &PageId::new(),
        &page,
        &VisionContext::default(),
    )
    .await;
    assert!(
        matches!(outcome, IntentOutcome::Completed { .. }),
        "the extension prompt was not pressed: {outcome:?}"
    );
    assert!(*page.filled.lock().expect("fill state"));
}

#[tokio::test]
async fn without_a_located_prompt_the_fill_still_asks_for_vision() {
    let page = ExtensionPage {
        prompt: None,
        filled: Mutex::new(false),
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
