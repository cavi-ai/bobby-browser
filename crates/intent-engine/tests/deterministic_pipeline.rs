use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use dom_engine::{Candidate, CandidateState};
use intent_engine::{IntentBrowser, IntentEngine, IntentOutcome, VisionContext};
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandError, ErrorCode, Evidence, IntentCommand,
    IntentHints, IntentResolutionPath, LocateIntent, PageId, TargetSpec, TypeTextCommand,
    UploadFilesCommand, WaitCondition, WaitForCommand, WaitForStateIntent, WaitUntil,
};

struct FakeBrowser {
    candidates: Arc<Vec<Candidate>>,
    wait_ok: bool,
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
        if self.wait_ok {
            Ok(vec![Evidence::Wait {
                condition: command.condition.clone(),
                elapsed_ms: 12,
                observations: 1,
                excluded_classes: Vec::new(),
                observed: None,
            }])
        } else {
            Err(CommandError {
                code: ErrorCode::WaitConditionTimedOut,
                message: "wait timed out".into(),
                layer: types::ErrorLayer::Page,
                retryable: true,
            })
        }
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
        id: format!("btn-{name}"),
        css: Some(format!("[data-name=\"{name}\"]")),
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

fn locate_continue() -> IntentCommand {
    IntentCommand::Locate(LocateIntent {
        purpose: "Continue".into(),
        hints: IntentHints {
            role: Some("button".into()),
            ..IntentHints::default()
        },
    })
}

fn locate_with_purpose(purpose: &str) -> IntentCommand {
    IntentCommand::Locate(LocateIntent {
        purpose: purpose.into(),
        hints: IntentHints {
            role: Some("button".into()),
            ..IntentHints::default()
        },
    })
}

#[tokio::test]
async fn locate_resolves_single_candidate_deterministically() {
    let browser = FakeBrowser {
        candidates: Arc::new(vec![button("Continue")]),
        wait_ok: true,
    };
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &locate_continue(),
        &page_id,
        &browser,
        &VisionContext::default(),
    )
    .await;

    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    assert!(
        evidence
            .iter()
            .any(|item| matches!(item, Evidence::Resolution { .. })),
        "missing Resolution evidence: {evidence:?}"
    );
    let record = evidence.iter().find_map(|item| match item {
        Evidence::IntentExecution { record } => Some(record),
        _ => None,
    });
    let record = record.expect("IntentExecution evidence");
    assert_eq!(record.intent_kind, "locate");
    assert_eq!(record.purpose.as_deref(), Some("Continue"));
    assert_eq!(record.resolution_path, IntentResolutionPath::Deterministic);
    assert_eq!(record.verification, "resolved");
}

#[tokio::test]
async fn locate_uses_descriptive_purpose_to_break_a_unique_role_only_tie() {
    let browser = FakeBrowser {
        candidates: Arc::new(vec![
            button("Customer document"),
            button("Upload document"),
            button("Confirm document preview"),
        ]),
        wait_ok: true,
    };
    let outcome = IntentEngine::execute(
        &locate_with_purpose("Confirm button inside the document preview iframe"),
        &PageId::new(),
        &browser,
        &VisionContext::default(),
    )
    .await;

    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    let Evidence::Resolution { fingerprint, .. } = evidence
        .iter()
        .find(|item| matches!(item, Evidence::Resolution { .. }))
        .expect("resolution evidence")
    else {
        unreachable!()
    };
    assert_eq!(
        fingerprint.name.as_deref(),
        Some("Confirm document preview")
    );
}

#[tokio::test]
async fn locate_uses_a_single_word_purpose_for_a_unique_exact_name() {
    let browser = FakeBrowser {
        candidates: Arc::new(vec![button("Continue"), button("Resume")]),
        wait_ok: true,
    };
    let outcome = IntentEngine::execute(
        &locate_with_purpose("Continue"),
        &PageId::new(),
        &browser,
        &VisionContext::default(),
    )
    .await;

    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    assert!(evidence.iter().any(|item| matches!(
        item,
        Evidence::Resolution { fingerprint, .. }
            if fingerprint.name.as_deref() == Some("Continue")
    )));
}

#[tokio::test]
async fn locate_uses_descriptive_purpose_for_a_unique_candidate_in_an_explicit_frame() {
    let browser = FakeBrowser {
        candidates: Arc::new(vec![button("Confirm document preview")]),
        wait_ok: true,
    };
    let intent = IntentCommand::Locate(LocateIntent {
        purpose: "Confirm button inside the document preview iframe".into(),
        hints: IntentHints {
            frame_path: vec![TargetSpec {
                css: Some("#document-preview".into()),
                ..TargetSpec::default()
            }],
            ..IntentHints::default()
        },
    });
    let outcome =
        IntentEngine::execute(&intent, &PageId::new(), &browser, &VisionContext::default()).await;

    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    assert!(evidence.iter().any(|item| matches!(
        item,
        Evidence::Resolution { fingerprint, .. }
            if fingerprint.name.as_deref() == Some("Confirm document preview")
    )));
}

#[tokio::test]
async fn locate_keeps_equal_purpose_overlap_ambiguous() {
    let browser = FakeBrowser {
        candidates: Arc::new(vec![
            button("Choose personal account"),
            button("Choose business account"),
        ]),
        wait_ok: true,
    };
    let outcome = IntentEngine::execute(
        &locate_with_purpose("Choose account button"),
        &PageId::new(),
        &browser,
        &VisionContext::default(),
    )
    .await;

    let IntentOutcome::Failed { error, evidence } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::VisionAssistDenied);
    let record = evidence.iter().find_map(|item| match item {
        Evidence::IntentExecution { record } => Some(record),
        _ => None,
    });
    assert_eq!(
        record.expect("intent evidence").verification,
        "targetAmbiguous"
    );
}

#[tokio::test]
async fn locate_zero_candidates_is_vision_assist_denied_when_gates_closed() {
    let browser = FakeBrowser {
        candidates: Arc::new(vec![]),
        wait_ok: true,
    };
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &locate_continue(),
        &page_id,
        &browser,
        &VisionContext::default(),
    )
    .await;

    let IntentOutcome::Failed { error, evidence } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    // Stuck taxonomy allows escalation, but deny-by-default gates are closed.
    assert_eq!(error.code, ErrorCode::VisionAssistDenied);
    let record = evidence.iter().find_map(|item| match item {
        Evidence::IntentExecution { record } => Some(record),
        _ => None,
    });
    let record = record.expect("IntentExecution on stuck failure");
    assert_eq!(record.resolution_path, IntentResolutionPath::Deterministic);
    assert_eq!(record.verification, "targetNotFound");
    assert!(record.vision_proposal_sha256.is_none());
}

#[tokio::test]
async fn wait_for_state_success_is_deterministic() {
    let browser = FakeBrowser {
        candidates: Arc::new(vec![]),
        wait_ok: true,
    };
    let page_id = PageId::new();
    let intent = IntentCommand::WaitForState(WaitForStateIntent {
        condition: WaitCondition::Document {
            ready: WaitUntil::Interactive,
        },
        timeout_ms: 1_000,
    });
    let outcome =
        IntentEngine::execute(&intent, &page_id, &browser, &VisionContext::default()).await;

    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    assert!(
        evidence
            .iter()
            .any(|item| matches!(item, Evidence::Wait { .. })),
        "missing Wait evidence: {evidence:?}"
    );
    let record = evidence.iter().find_map(|item| match item {
        Evidence::IntentExecution { record } => Some(record),
        _ => None,
    });
    let record = record.expect("IntentExecution evidence");
    assert_eq!(record.intent_kind, "waitForState");
    assert_eq!(record.resolution_path, IntentResolutionPath::Deterministic);
    assert_eq!(record.verification, "waitSatisfied");
    assert_eq!(record.wait_elapsed_ms, Some(12));
}
