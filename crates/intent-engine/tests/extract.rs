use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dom_engine::{Candidate, CandidateState};
use intent_engine::{
    IntentBrowser, IntentEngine, IntentOutcome, VisionAction, VisionAssist, VisionContext,
    VisionProposal, VisionProposeRequest,
};
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandError, ErrorCode, ErrorLayer, Evidence,
    ExtractField, ExtractIntent, ExtractValueKind, IntentCommand, IntentHints,
    IntentResolutionPath, PageId, TargetSpec, TypeTextCommand, UploadFilesCommand, WaitForCommand,
};

#[derive(Default)]
struct FakeBrowser {
    /// Popped once per `collect_candidates` call, one entry per field in
    /// declaration order.
    candidate_responses: Arc<Mutex<VecDeque<Vec<Candidate>>>>,
    screenshot_png: Vec<u8>,
}

#[async_trait]
impl IntentBrowser for FakeBrowser {
    async fn collect_candidates(
        &self,
        _page_id: &PageId,
        _target: &TargetSpec,
    ) -> Result<Vec<Candidate>, CommandError> {
        Ok(self
            .candidate_responses
            .lock()
            .expect("responses")
            .pop_front()
            .unwrap_or_default())
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

struct FakeVision {
    proposal: VisionProposal,
}

#[async_trait]
impl VisionAssist for FakeVision {
    async fn propose(
        &self,
        _request: VisionProposeRequest,
    ) -> Result<VisionProposal, CommandError> {
        Ok(self.proposal.clone())
    }
}

fn unsupported(op: &str) -> CommandError {
    CommandError {
        code: ErrorCode::Internal,
        message: format!("{op} not supported by fake browser"),
        layer: ErrorLayer::Page,
        retryable: false,
    }
}

fn candidate(name: &str, text: &str, attributes: BTreeMap<String, String>) -> Candidate {
    Candidate {
        id: name.into(),
        css: Some(format!("#{name}")),
        test_id: None,
        role: None,
        name: Some(name.into()),
        label: None,
        text: text.into(),
        attributes,
        state: CandidateState {
            attached: true,
            visible: true,
            enabled: true,
        },
    }
}

const DISPLAY_NAME_FIELD: &str = "displayName";
const PROFILE_LINK_FIELD: &str = "profileLink";

fn field(name: &str, purpose: &str, value: ExtractValueKind) -> ExtractField {
    ExtractField {
        name: name.into(),
        purpose: purpose.into(),
        hints: IntentHints::default(),
        value,
    }
}

fn extract(fields: Vec<ExtractField>) -> IntentCommand {
    IntentCommand::Extract(ExtractIntent {
        purpose: "Profile summary".into(),
        fields,
    })
}

fn find_extraction<'a>(evidence: &'a [Evidence], field_name: &str) -> &'a Evidence {
    evidence
        .iter()
        .find(|item| matches!(item, Evidence::Extraction { field, .. } if field == field_name))
        .unwrap_or_else(|| panic!("no Extraction evidence for field {field_name}"))
}

#[tokio::test]
async fn extract_resolves_every_field_deterministically_and_reads_declared_value_kinds() {
    let mut link_attrs = BTreeMap::new();
    link_attrs.insert("href".to_owned(), "/users/42".to_owned());
    let browser = FakeBrowser {
        candidate_responses: Arc::new(Mutex::new(VecDeque::from([
            vec![candidate(
                DISPLAY_NAME_FIELD,
                "Ada Lovelace",
                BTreeMap::new(),
            )],
            vec![candidate(PROFILE_LINK_FIELD, "View profile", link_attrs)],
        ]))),
        ..FakeBrowser::default()
    };
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &extract(vec![
            field(DISPLAY_NAME_FIELD, "Ada Lovelace", ExtractValueKind::Text),
            field(PROFILE_LINK_FIELD, "View profile", ExtractValueKind::Href),
        ]),
        &page_id,
        &browser,
        &VisionContext::default(),
    )
    .await;

    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };

    let Evidence::Extraction {
        value,
        resolution_path,
        error_code,
        ..
    } = find_extraction(&evidence, DISPLAY_NAME_FIELD)
    else {
        unreachable!()
    };
    assert_eq!(value.as_deref(), Some("Ada Lovelace"));
    assert_eq!(*resolution_path, IntentResolutionPath::Deterministic);
    assert_eq!(*error_code, None);

    let Evidence::Extraction { value, .. } = find_extraction(&evidence, PROFILE_LINK_FIELD) else {
        unreachable!()
    };
    assert_eq!(value.as_deref(), Some("/users/42"));

    let record = evidence.iter().find_map(|item| match item {
        Evidence::IntentExecution { record } => Some(record),
        _ => None,
    });
    let record = record.expect("IntentExecution evidence");
    assert_eq!(record.intent_kind, "extract");
    assert_eq!(record.verification, "extracted");
    assert!(
        evidence
            .iter()
            .filter(|item| matches!(item, Evidence::Resolution { .. }))
            .count()
            >= 2
    );
}

#[tokio::test]
async fn extract_marks_a_field_missing_without_failing_the_whole_command() {
    let browser = FakeBrowser {
        candidate_responses: Arc::new(Mutex::new(VecDeque::from([
            vec![candidate(
                DISPLAY_NAME_FIELD,
                "Ada Lovelace",
                BTreeMap::new(),
            )],
            vec![],
        ]))),
        ..FakeBrowser::default()
    };
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &extract(vec![
            field(DISPLAY_NAME_FIELD, "Ada Lovelace", ExtractValueKind::Text),
            field(PROFILE_LINK_FIELD, "View profile", ExtractValueKind::Href),
        ]),
        &page_id,
        &browser,
        &VisionContext::default(),
    )
    .await;

    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };

    let Evidence::Extraction {
        value,
        error_code,
        resolution_path,
        ..
    } = find_extraction(&evidence, PROFILE_LINK_FIELD)
    else {
        unreachable!()
    };
    assert_eq!(*value, None);
    // No vision configured (default `VisionContext`), so the escalatable
    // `TargetNotFound` reason surfaces as `VisionAssistDenied` — same
    // convention `DismissObstructionIntent` uses for a missing target.
    assert_eq!(*error_code, Some(ErrorCode::VisionAssistDenied));
    assert_eq!(*resolution_path, IntentResolutionPath::Deterministic);

    let Evidence::Extraction { value, .. } = find_extraction(&evidence, DISPLAY_NAME_FIELD) else {
        unreachable!()
    };
    assert_eq!(value.as_deref(), Some("Ada Lovelace"));

    let record = evidence.iter().find_map(|item| match item {
        Evidence::IntentExecution { record } => Some(record),
        _ => None,
    });
    let record = record.expect("IntentExecution evidence");
    assert!(record.verification.starts_with("extractedPartial:missing="));
    assert!(record.verification.contains(PROFILE_LINK_FIELD));
}

#[tokio::test]
async fn extract_missing_field_is_vision_assist_denied_when_gates_closed() {
    let browser = FakeBrowser {
        candidate_responses: Arc::new(Mutex::new(VecDeque::from([vec![]]))),
        ..FakeBrowser::default()
    };
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &extract(vec![field(
            PROFILE_LINK_FIELD,
            "View profile",
            ExtractValueKind::Href,
        )]),
        &page_id,
        &browser,
        &VisionContext::default(),
    )
    .await;

    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    let Evidence::Extraction { error_code, .. } = find_extraction(&evidence, PROFILE_LINK_FIELD)
    else {
        unreachable!()
    };
    assert_eq!(*error_code, Some(ErrorCode::VisionAssistDenied));
}

#[tokio::test]
async fn extract_escalates_missing_field_to_vision_and_uses_the_proposed_value() {
    let browser = FakeBrowser {
        candidate_responses: Arc::new(Mutex::new(VecDeque::from([vec![]]))),
        screenshot_png: b"png".to_vec(),
    };
    let assist = Arc::new(FakeVision {
        proposal: VisionProposal {
            confidence: 0.9,
            action: VisionAction::ExtractValue {
                value: "Ada Lovelace".into(),
            },
        },
    });
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &extract(vec![field(
            DISPLAY_NAME_FIELD,
            "Ada Lovelace",
            ExtractValueKind::Text,
        )]),
        &page_id,
        &browser,
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(assist),
            proposals: None,
            defer_escalation: false,
        },
    )
    .await;

    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    let Evidence::Extraction {
        value,
        resolution_path,
        error_code,
        ..
    } = find_extraction(&evidence, DISPLAY_NAME_FIELD)
    else {
        unreachable!()
    };
    assert_eq!(value.as_deref(), Some("Ada Lovelace"));
    assert_eq!(*resolution_path, IntentResolutionPath::VisionFallback);
    assert_eq!(*error_code, None);

    let record = evidence.iter().find_map(|item| match item {
        Evidence::IntentExecution { record } => Some(record),
        _ => None,
    });
    assert_eq!(record.expect("record").verification, "extracted");
}

#[tokio::test]
async fn extract_reports_field_missing_when_vision_confidence_is_below_floor() {
    let browser = FakeBrowser {
        candidate_responses: Arc::new(Mutex::new(VecDeque::from([vec![]]))),
        screenshot_png: b"png".to_vec(),
    };
    let assist = Arc::new(FakeVision {
        proposal: VisionProposal {
            confidence: 0.1,
            action: VisionAction::ExtractValue {
                value: "guess".into(),
            },
        },
    });
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &extract(vec![field(
            DISPLAY_NAME_FIELD,
            "Ada Lovelace",
            ExtractValueKind::Text,
        )]),
        &page_id,
        &browser,
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(assist),
            proposals: None,
            defer_escalation: false,
        },
    )
    .await;

    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    let Evidence::Extraction {
        value, error_code, ..
    } = find_extraction(&evidence, DISPLAY_NAME_FIELD)
    else {
        unreachable!()
    };
    assert_eq!(*value, None);
    assert_eq!(*error_code, Some(ErrorCode::VisionAssistFailed));
}

#[tokio::test]
async fn extract_resolved_field_with_absent_attribute_reports_value_none_without_error_code() {
    let browser = FakeBrowser {
        candidate_responses: Arc::new(Mutex::new(VecDeque::from([vec![candidate(
            PROFILE_LINK_FIELD,
            "View profile",
            BTreeMap::new(),
        )]]))),
        ..FakeBrowser::default()
    };
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &extract(vec![field(
            PROFILE_LINK_FIELD,
            "View profile",
            ExtractValueKind::Href,
        )]),
        &page_id,
        &browser,
        &VisionContext::default(),
    )
    .await;

    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    let Evidence::Extraction {
        value,
        error_code,
        resolution_path,
        ..
    } = find_extraction(&evidence, PROFILE_LINK_FIELD)
    else {
        unreachable!()
    };
    assert_eq!(*value, None, "candidate resolved but has no href attribute");
    assert_eq!(*error_code, None);
    assert_eq!(*resolution_path, IntentResolutionPath::Deterministic);
}
