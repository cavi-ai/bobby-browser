use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dom_engine::{Candidate, CandidateState};
use intent_engine::{IntentBrowser, IntentEngine, IntentOutcome, VisionContext};
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandError, ErrorCode, Evidence, IntentCommand,
    IntentHints, IntentResolutionPath, PageId, SubmitAndVerifyIntent, TargetSpec, TextMatch,
    TypeTextCommand, UploadFilesCommand, WaitCondition, WaitForCommand,
};

#[derive(Default)]
struct CallLog {
    clicks: Vec<ClickCommand>,
    waits: Vec<WaitForCommand>,
}

struct FakeBrowser {
    candidates: Arc<Vec<Candidate>>,
    calls: Arc<Mutex<CallLog>>,
    click_evidence: Vec<Evidence>,
    click_error: Option<CommandError>,
    wait_evidence: Vec<Evidence>,
    wait_error: Option<CommandError>,
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
        command: &ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.calls
            .lock()
            .expect("call log")
            .clicks
            .push(command.clone());
        if let Some(error) = &self.click_error {
            return Err(error.clone());
        }
        Ok(self.click_evidence.clone())
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
        command: &WaitForCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.calls
            .lock()
            .expect("call log")
            .waits
            .push(command.clone());
        if let Some(error) = &self.wait_error {
            return Err(error.clone());
        }
        Ok(self.wait_evidence.clone())
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

fn button(name: &str) -> Candidate {
    Candidate {
        id: name.into(),
        css: Some(format!("#{name}")),
        test_id: None,
        role: Some("button".into()),
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

fn submit(purpose: &str, role: Option<&str>, expected_state: WaitForCommand) -> IntentCommand {
    IntentCommand::SubmitAndVerify(SubmitAndVerifyIntent {
        purpose: purpose.into(),
        hints: IntentHints {
            role: role.map(str::to_owned),
            ..IntentHints::default()
        },
        expected_state,
    })
}

fn thanks_wait() -> WaitForCommand {
    WaitForCommand {
        condition: WaitCondition::Url {
            matcher: TextMatch::Contains("/thanks".into()),
        },
        timeout_ms: 5_000,
    }
}

#[tokio::test]
async fn submit_and_verify_clicks_boundary_then_waits() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let expected_state = thanks_wait();
    let browser = FakeBrowser {
        candidates: Arc::new(vec![button("Submit application")]),
        calls: Arc::clone(&calls),
        click_evidence: vec![Evidence::Element {
            selector: "#Submit application".into(),
            text: None,
        }],
        click_error: None,
        wait_evidence: vec![Evidence::Wait {
            condition: expected_state.condition.clone(),
            elapsed_ms: 12,
            observations: 1,
            excluded_classes: Vec::new(),
            observed: None,
        }],
        wait_error: None,
    };
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &submit("Submit application", Some("button"), expected_state.clone()),
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
        assert_eq!(log.clicks.len(), 1);
        assert!(log.clicks[0].boundary);
        assert_eq!(log.clicks[0].expected_url, None);
        assert_eq!(log.waits.len(), 1);
        assert_eq!(log.waits[0], expected_state);
    }
    let record = evidence.iter().find_map(|item| match item {
        Evidence::IntentExecution { record } => Some(record),
        _ => None,
    });
    let record = record.expect("IntentExecution evidence");
    assert_eq!(record.intent_kind, "submitAndVerify");
    assert_eq!(record.purpose.as_deref(), Some("Submit application"));
    assert_eq!(record.resolution_path, IntentResolutionPath::Deterministic);
    assert_eq!(record.verification, "submitted");
    assert_eq!(record.wait_elapsed_ms, Some(12));
    assert!(evidence
        .iter()
        .any(|item| matches!(item, Evidence::Resolution { .. })));
}

#[tokio::test]
async fn submit_and_verify_sets_expected_url_from_exact_wait() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let expected_state = WaitForCommand {
        condition: WaitCondition::Url {
            matcher: TextMatch::Exact("https://example.test/thanks".into()),
        },
        timeout_ms: 1_000,
    };
    let browser = FakeBrowser {
        candidates: Arc::new(vec![button("Submit")]),
        calls: Arc::clone(&calls),
        click_evidence: vec![Evidence::Element {
            selector: "#Submit".into(),
            text: None,
        }],
        click_error: None,
        wait_evidence: vec![Evidence::Wait {
            condition: expected_state.condition.clone(),
            elapsed_ms: 3,
            observations: 1,
            excluded_classes: Vec::new(),
            observed: None,
        }],
        wait_error: None,
    };
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &submit("Submit", Some("button"), expected_state),
        &page_id,
        &browser,
        &VisionContext::default(),
    )
    .await;

    assert!(matches!(outcome, IntentOutcome::Completed { .. }));
    let log = calls.lock().expect("call log");
    assert_eq!(
        log.clicks[0].expected_url.as_deref(),
        Some("https://example.test/thanks")
    );
    assert!(log.clicks[0].boundary);
}

#[tokio::test]
async fn submit_and_verify_missing_target_is_stuck() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let browser = FakeBrowser {
        candidates: Arc::new(Vec::new()),
        calls: Arc::clone(&calls),
        click_evidence: Vec::new(),
        click_error: None,
        wait_evidence: Vec::new(),
        wait_error: None,
    };
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &submit("Submit application", Some("button"), thanks_wait()),
        &page_id,
        &browser,
        &VisionContext::default(),
    )
    .await;

    let IntentOutcome::Failed { error, evidence } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::VisionAssistDenied);
    {
        let log = calls.lock().expect("call log");
        assert!(log.clicks.is_empty());
        assert!(log.waits.is_empty());
    }
    let record = evidence.iter().find_map(|item| match item {
        Evidence::IntentExecution { record } => Some(record),
        _ => None,
    });
    let record = record.expect("IntentExecution on stuck");
    assert_eq!(record.verification, "targetNotFound");
    assert_eq!(record.intent_kind, "submitAndVerify");
}

#[tokio::test]
async fn submit_and_verify_checks_post_state_after_navigation_destroys_click_context() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let expected_state = thanks_wait();
    let browser = FakeBrowser {
        candidates: Arc::new(vec![button("Submit")]),
        calls: Arc::clone(&calls),
        click_evidence: Vec::new(),
        click_error: Some(CommandError {
            code: ErrorCode::BrowserCommandFailed,
            message: "Error -32000: Cannot find context with specified id".into(),
            layer: types::ErrorLayer::Driver,
            retryable: false,
        }),
        wait_evidence: vec![Evidence::Wait {
            condition: expected_state.condition.clone(),
            elapsed_ms: 4,
            observations: 1,
            excluded_classes: Vec::new(),
            observed: Some("https://example.test/thanks".into()),
        }],
        wait_error: None,
    };

    let outcome = IntentEngine::execute(
        &submit("Submit", Some("button"), expected_state),
        &PageId::new(),
        &browser,
        &VisionContext::default(),
    )
    .await;

    assert!(
        matches!(outcome, IntentOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let log = calls.lock().expect("call log");
    assert_eq!(log.clicks.len(), 1);
    assert_eq!(log.waits.len(), 1);
}

#[tokio::test]
async fn submit_and_verify_wait_timeout_after_landed_click_is_not_a_resubmit_invitation() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let expected_state = WaitForCommand {
        condition: WaitCondition::Element {
            target: Box::new(TargetSpec {
                role: Some("alert".into()),
                ..TargetSpec::default()
            }),
            state: types::ElementState::Visible,
        },
        timeout_ms: 1_000,
    };
    let browser = FakeBrowser {
        candidates: Arc::new(vec![button("Submit")]),
        calls: Arc::clone(&calls),
        click_evidence: vec![Evidence::Element {
            selector: "#Submit".into(),
            text: None,
        }],
        click_error: None,
        wait_evidence: Vec::new(),
        wait_error: Some(CommandError {
            code: ErrorCode::WaitConditionTimedOut,
            message: "condition did not hold within 1000ms".into(),
            layer: types::ErrorLayer::Page,
            retryable: true,
        }),
    };

    let outcome = IntentEngine::execute(
        &submit("Submit", Some("button"), expected_state),
        &PageId::new(),
        &browser,
        &VisionContext::default(),
    )
    .await;

    let IntentOutcome::Failed { error, evidence } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    // The click landed; the failure is verification, and a blind retry would
    // duplicate the POST.
    assert_eq!(error.code, ErrorCode::VerificationFailed);
    assert!(!error.retryable);
    assert!(
        error.message.contains("submit click landed"),
        "{error:?}"
    );
    assert!(
        error.message.contains("Do not resubmit blindly"),
        "{error:?}"
    );
    assert!(error.message.contains("element"), "{error:?}");
    let record = evidence.iter().find_map(|item| match item {
        Evidence::IntentExecution { record } => Some(record),
        _ => None,
    });
    assert_eq!(record.expect("IntentExecution").verification, "verifyFailed");
    let log = calls.lock().expect("call log");
    assert_eq!(log.clicks.len(), 1);
    assert_eq!(log.waits.len(), 1);
}
