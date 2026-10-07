use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dom_engine::{Candidate, CandidateState};
use intent_engine::{IntentBrowser, IntentEngine, IntentOutcome, VisionContext};
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandError, ErrorCode, Evidence, FollowIntent,
    IntentCommand, IntentHints, IntentResolutionPath, PageId, TargetSpec, TextMatch,
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

fn link(name: &str) -> Candidate {
    Candidate {
        id: name.into(),
        css: Some(format!("#{name}")),
        tag: None,
        test_id: None,
        role: Some("link".into()),
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

fn follow(
    purpose: &str,
    role: Option<&str>,
    expected_destination: WaitForCommand,
    boundary: bool,
) -> IntentCommand {
    IntentCommand::Follow(FollowIntent {
        purpose: purpose.into(),
        hints: IntentHints {
            role: role.map(str::to_owned),
            ..IntentHints::default()
        },
        expected_destination,
        boundary,
    })
}

fn details_wait() -> WaitForCommand {
    WaitForCommand {
        condition: WaitCondition::Url {
            matcher: TextMatch::Contains("/details".into()),
        },
        timeout_ms: 5_000,
    }
}

#[tokio::test]
async fn follow_clicks_target_then_waits_for_destination_without_boundary() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let expected_destination = details_wait();
    let browser = FakeBrowser {
        candidates: Arc::new(vec![link("Details")]),
        calls: Arc::clone(&calls),
        click_evidence: vec![Evidence::Element {
            selector: "#Details".into(),
            text: None,
        }],
        wait_evidence: vec![Evidence::Wait {
            condition: expected_destination.condition.clone(),
            elapsed_ms: 8,
            observations: 1,
            excluded_classes: Vec::new(),
            observed: None,
        }],
        wait_error: None,
    };
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &follow("Details", Some("link"), expected_destination.clone(), false),
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
        assert!(
            !log.clicks[0].boundary,
            "boundary:false must not be escalated to a boundary click"
        );
        assert_eq!(log.clicks[0].expected_url, None);
        assert_eq!(log.waits.len(), 1);
        assert_eq!(log.waits[0], expected_destination);
    }
    let record = evidence.iter().find_map(|item| match item {
        Evidence::IntentExecution { record } => Some(record),
        _ => None,
    });
    let record = record.expect("IntentExecution evidence");
    assert_eq!(record.intent_kind, "follow");
    assert_eq!(record.purpose.as_deref(), Some("Details"));
    assert_eq!(record.resolution_path, IntentResolutionPath::Deterministic);
    assert_eq!(record.verification, "followed");
    assert_eq!(record.wait_elapsed_ms, Some(8));
    assert!(evidence
        .iter()
        .any(|item| matches!(item, Evidence::Resolution { .. })));
}

#[tokio::test]
async fn follow_keeps_the_ordinal_for_a_candidate_without_a_css_identity() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let expected_destination = details_wait();
    let semantic = |id: &str| Candidate {
        css: None,
        id: id.into(),
        ..link("Widget Pro")
    };
    let browser = FakeBrowser {
        candidates: Arc::new(vec![semantic("a"), semantic("b")]),
        calls: Arc::clone(&calls),
        click_evidence: Vec::new(),
        wait_evidence: Vec::new(),
        wait_error: None,
    };
    let command = IntentCommand::Follow(FollowIntent {
        purpose: "Open the second Widget Pro".into(),
        hints: IntentHints {
            role: Some("link".into()),
            accessible_name: Some("Widget Pro".into()),
            ordinal: Some(1),
            ..IntentHints::default()
        },
        expected_destination,
        boundary: false,
    });
    let outcome = IntentEngine::execute(
        &command,
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
    assert_eq!(log.clicks[0].target.as_ref().unwrap().ordinal, Some(1));
}

#[tokio::test]
async fn follow_never_treats_state_attributes_as_identity() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let mut candidate = link("Details");
    candidate.css = None;
    candidate.attributes = BTreeMap::from([
        ("aria-invalid".to_owned(), "true".to_owned()),
        ("aria-expanded".to_owned(), "false".to_owned()),
        ("checked".to_owned(), "true".to_owned()),
        ("disabled".to_owned(), "true".to_owned()),
        ("value".to_owned(), "typed".to_owned()),
        ("name".to_owned(), "details".to_owned()),
    ]);
    let browser = FakeBrowser {
        candidates: Arc::new(vec![candidate]),
        calls: Arc::clone(&calls),
        click_evidence: Vec::new(),
        wait_evidence: Vec::new(),
        wait_error: None,
    };
    let outcome = IntentEngine::execute(
        &follow("Details", Some("link"), details_wait(), false),
        &PageId::new(),
        &browser,
        &VisionContext::default(),
    )
    .await;

    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    let expected = BTreeMap::from([("name".to_owned(), "details".to_owned())]);
    let log = calls.lock().expect("call log");
    assert_eq!(log.clicks[0].target.as_ref().unwrap().attributes, expected);
    let fingerprint = evidence
        .iter()
        .find_map(|item| match item {
            Evidence::Resolution { fingerprint, .. } => Some(fingerprint),
            _ => None,
        })
        .expect("resolution evidence");
    assert_eq!(fingerprint.stable_attributes, expected);
}

#[tokio::test]
async fn follow_forwards_boundary_true_verbatim_to_the_click_command() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let expected_destination = WaitForCommand {
        condition: WaitCondition::Url {
            matcher: TextMatch::Contains("/signed-out".into()),
        },
        timeout_ms: 5_000,
    };
    let browser = FakeBrowser {
        candidates: Arc::new(vec![link("Sign out")]),
        calls: Arc::clone(&calls),
        click_evidence: vec![Evidence::Element {
            selector: "#Sign out".into(),
            text: None,
        }],
        wait_evidence: vec![Evidence::Wait {
            condition: expected_destination.condition.clone(),
            elapsed_ms: 4,
            observations: 1,
            excluded_classes: Vec::new(),
            observed: None,
        }],
        wait_error: None,
    };
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &follow("Sign out", Some("link"), expected_destination, true),
        &page_id,
        &browser,
        &VisionContext::default(),
    )
    .await;

    assert!(matches!(outcome, IntentOutcome::Completed { .. }));
    let log = calls.lock().expect("call log");
    assert!(
        log.clicks[0].boundary,
        "boundary:true must be forwarded to the click primitive"
    );
}

#[tokio::test]
async fn follow_sets_expected_url_from_exact_wait() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let expected_destination = WaitForCommand {
        condition: WaitCondition::Url {
            matcher: TextMatch::Exact("https://example.test/details".into()),
        },
        timeout_ms: 1_000,
    };
    let browser = FakeBrowser {
        candidates: Arc::new(vec![link("Details")]),
        calls: Arc::clone(&calls),
        click_evidence: vec![Evidence::Element {
            selector: "#Details".into(),
            text: None,
        }],
        wait_evidence: vec![Evidence::Wait {
            condition: expected_destination.condition.clone(),
            elapsed_ms: 3,
            observations: 1,
            excluded_classes: Vec::new(),
            observed: None,
        }],
        wait_error: None,
    };
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &follow("Details", Some("link"), expected_destination, false),
        &page_id,
        &browser,
        &VisionContext::default(),
    )
    .await;

    assert!(matches!(outcome, IntentOutcome::Completed { .. }));
    let log = calls.lock().expect("call log");
    assert_eq!(
        log.clicks[0].expected_url.as_deref(),
        Some("https://example.test/details")
    );
}

#[tokio::test]
async fn follow_missing_target_is_stuck() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let browser = FakeBrowser {
        candidates: Arc::new(Vec::new()),
        calls: Arc::clone(&calls),
        click_evidence: Vec::new(),
        wait_evidence: Vec::new(),
        wait_error: None,
    };
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &follow("Details", Some("link"), details_wait(), false),
        &page_id,
        &browser,
        &VisionContext::default(),
    )
    .await;

    let IntentOutcome::Failed { error, evidence } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::TargetNotFound);
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
    assert_eq!(record.intent_kind, "follow");
}

/// The click landed; the post-click wait to verify the destination found
/// more than one matching candidate. That is not a retryable ambiguity —
/// the activation already happened — so the outcome must be `Failed` with
/// `VerificationFailed`, not the raw `TargetAmbiguous`, and the click
/// evidence must still be present so a reader can see the click ran.
#[tokio::test]
async fn follow_recodes_ambiguous_postclick_wait_as_verification_failed() {
    let calls = Arc::new(Mutex::new(CallLog::default()));
    let expected_destination = details_wait();
    let browser = FakeBrowser {
        candidates: Arc::new(vec![link("Details")]),
        calls: Arc::clone(&calls),
        click_evidence: vec![Evidence::Element {
            selector: "#Details".into(),
            text: None,
        }],
        wait_evidence: Vec::new(),
        wait_error: Some(CommandError {
            code: ErrorCode::TargetAmbiguous,
            message: "target is ambiguous: paragraph \"Step 2 of 3\" score=30".into(),
            layer: types::ErrorLayer::Page,
            retryable: true,
        }),
    };
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &follow("Details", Some("link"), expected_destination, false),
        &page_id,
        &browser,
        &VisionContext::default(),
    )
    .await;

    let IntentOutcome::Failed { error, evidence } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::VerificationFailed);
    assert!(
        error.message.contains("landed"),
        "message should say the click landed: {}",
        error.message
    );
    assert!(!error.retryable);
    assert!(
        evidence
            .iter()
            .any(|item| matches!(item, Evidence::Element { .. })),
        "click evidence must be present: {evidence:?}"
    );
    let log = calls.lock().expect("call log");
    assert_eq!(log.clicks.len(), 1, "the click ran exactly once");
}
