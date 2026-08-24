use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dom_engine::{Candidate, CandidateState};
use intent_engine::{
    IntentBrowser, IntentEngine, IntentOutcome, VisionAction, VisionAssist, VisionContext,
    VisionCorpus, VisionPromptCandidate, VisionPromptContext, VisionProposal, VisionProposeRequest,
};
use observability::{OperationalMetrics, ProviderMode};
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
    requests: Arc<Mutex<Vec<VisionProposeRequest>>>,
    metrics: Option<OperationalMetrics>,
}

#[async_trait]
impl VisionAssist for FakeVision {
    async fn propose(&self, request: VisionProposeRequest) -> Result<VisionProposal, CommandError> {
        self.requests.lock().expect("requests").push(request);
        Ok(self.proposal.clone())
    }

    fn operational_metrics(&self) -> Option<(OperationalMetrics, ProviderMode)> {
        self.metrics
            .as_ref()
            .map(|metrics| (metrics.clone(), ProviderMode::DirectLocal))
    }
}

fn fake_vision(proposal: VisionProposal) -> Arc<FakeVision> {
    Arc::new(FakeVision {
        proposal,
        requests: Arc::default(),
        metrics: None,
    })
}

fn metric_vision(proposal: VisionProposal, metrics: OperationalMetrics) -> Arc<FakeVision> {
    Arc::new(FakeVision {
        proposal,
        requests: Arc::default(),
        metrics: Some(metrics),
    })
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
        frame_path: Vec::new(),
    }
}

fn candidate_with_role(
    role: &str,
    name: &str,
    text: &str,
    attributes: BTreeMap<String, String>,
) -> Candidate {
    Candidate {
        role: Some(role.into()),
        ..candidate(name, text, attributes)
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
    let assist = fake_vision(VisionProposal {
        confidence: 0.9,
        action: VisionAction::ExtractValue {
            value: "Ada Lovelace".into(),
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
            prompt_context: None,
            corpus: None,
            context_store: None,
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
    let assist = fake_vision(VisionProposal {
        confidence: 0.1,
        action: VisionAction::ExtractValue {
            value: "guess".into(),
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
            prompt_context: None,
            corpus: None,
            context_store: None,
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

#[tokio::test]
async fn extract_from_candidate_reads_the_exact_provider_selected_candidate_for_each_value_kind() {
    let mut primary_attributes = BTreeMap::new();
    primary_attributes.insert("href".to_owned(), "/accounts/primary".to_owned());
    primary_attributes.insert("data-account".to_owned(), "primary".to_owned());
    let mut secondary_attributes = BTreeMap::new();
    secondary_attributes.insert("href".to_owned(), "/accounts/secondary".to_owned());
    secondary_attributes.insert("data-account".to_owned(), "secondary".to_owned());
    let candidates = vec![
        candidate_with_role(
            "link",
            "Primary account",
            "Account runtime-primary-text",
            primary_attributes,
        ),
        candidate_with_role(
            "link",
            "Secondary account",
            "Account runtime-secondary-text",
            secondary_attributes,
        ),
    ];
    let browser = FakeBrowser {
        candidate_responses: Arc::new(Mutex::new(VecDeque::from([
            candidates.clone(),
            candidates.clone(),
            candidates,
        ]))),
        screenshot_png: b"png".to_vec(),
    };
    let assist = fake_vision(VisionProposal {
        confidence: 0.9,
        action: VisionAction::ExtractFromCandidate { index: 1 },
    });
    let page_id = PageId::new();
    let outcome = IntentEngine::execute(
        &extract(vec![
            field("text", "Account", ExtractValueKind::Text),
            field("href", "Account", ExtractValueKind::Href),
            field(
                "attribute",
                "Account",
                ExtractValueKind::Attribute {
                    attribute: "data-account".into(),
                },
            ),
        ]),
        &page_id,
        &browser,
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(assist.clone()),
            proposals: None,
            defer_escalation: false,
            prompt_context: Some(VisionPromptContext {
                url: Some("https://example.test/accounts".into()),
                candidates: vec![VisionPromptCandidate {
                    role: "link".into(),
                    name: "Stale account".into(),
                    ordinal: None,
                }],
                recent_command_kinds: vec!["locate".into()],
            }),
            corpus: None,
            context_store: None,
        },
    )
    .await;

    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    let expected_prompt_candidates =
        vec![("link", "Primary account"), ("link", "Secondary account")];
    let requests = assist.requests.lock().expect("requests");
    assert_eq!(requests.len(), 3);
    for request in requests.iter() {
        let context = request.context.as_ref().expect("candidate context");
        assert_eq!(
            context.url.as_deref(),
            Some("https://example.test/accounts")
        );
        assert_eq!(context.recent_command_kinds, ["locate"]);
        assert_eq!(
            context
                .candidates
                .iter()
                .map(|candidate| (candidate.role.as_str(), candidate.name.as_str()))
                .collect::<Vec<_>>(),
            expected_prompt_candidates
        );
    }

    for (field_name, expected_value) in [
        ("text", "Account runtime-secondary-text"),
        ("href", "/accounts/secondary"),
        ("attribute", "secondary"),
    ] {
        let Evidence::Extraction {
            value,
            resolution_path,
            error_code,
            ..
        } = find_extraction(&evidence, field_name)
        else {
            unreachable!()
        };
        assert_eq!(value.as_deref(), Some(expected_value));
        assert_eq!(*resolution_path, IntentResolutionPath::VisionFallback);
        assert_eq!(*error_code, None);
    }
}

#[tokio::test]
async fn extract_from_candidate_uses_second_dom_candidate_when_provider_visible_semantics_duplicate(
) {
    let mut first_attributes = BTreeMap::new();
    first_attributes.insert("href".to_owned(), "/accounts/runtime-first".to_owned());
    first_attributes.insert("data-account".to_owned(), "runtime-first".to_owned());
    let mut second_attributes = BTreeMap::new();
    second_attributes.insert("href".to_owned(), "/accounts/runtime-second".to_owned());
    second_attributes.insert("data-account".to_owned(), "runtime-second".to_owned());
    let mut first = candidate_with_role(
        "link",
        "Account",
        "Account runtime-first-text",
        first_attributes,
    );
    first.id = "account-first-dom-id".into();
    first.css = Some("#account-first".into());
    let mut second = candidate_with_role(
        "link",
        "Account",
        "Account runtime-second-text",
        second_attributes,
    );
    second.id = "account-second-dom-id".into();
    second.css = Some("#account-second".into());
    let candidates = vec![first, second];
    let browser = FakeBrowser {
        candidate_responses: Arc::new(Mutex::new(VecDeque::from([
            candidates.clone(),
            candidates.clone(),
            candidates,
        ]))),
        screenshot_png: b"png".to_vec(),
    };
    let assist = fake_vision(VisionProposal {
        confidence: 0.9,
        action: VisionAction::ExtractFromCandidate { index: 1 },
    });
    let outcome = IntentEngine::execute(
        &extract(vec![
            field("text", "Account", ExtractValueKind::Text),
            field("href", "Account", ExtractValueKind::Href),
            field(
                "attribute",
                "Account",
                ExtractValueKind::Attribute {
                    attribute: "data-account".into(),
                },
            ),
        ]),
        &PageId::new(),
        &browser,
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(assist.clone()),
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
    let requests = assist.requests.lock().expect("requests");
    assert_eq!(requests.len(), 3);
    for request in requests.iter() {
        let context = request.context.as_ref().expect("candidate context");
        assert_eq!(
            context
                .candidates
                .iter()
                .map(|candidate| (candidate.role.as_str(), candidate.name.as_str()))
                .collect::<Vec<_>>(),
            [("link", "Account"), ("link", "Account")]
        );
    }

    for (field_name, expected_value) in [
        ("text", "Account runtime-second-text"),
        ("href", "/accounts/runtime-second"),
        ("attribute", "runtime-second"),
    ] {
        let Evidence::Extraction {
            value,
            resolution_path,
            error_code,
            ..
        } = find_extraction(&evidence, field_name)
        else {
            unreachable!()
        };
        assert_eq!(value.as_deref(), Some(expected_value));
        assert_eq!(*resolution_path, IntentResolutionPath::VisionFallback);
        assert_eq!(*error_code, None);
    }
}

#[tokio::test]
async fn extract_from_candidate_out_of_range_reports_vision_assist_failed() {
    let browser = FakeBrowser {
        candidate_responses: Arc::new(Mutex::new(VecDeque::from([ambiguous_extract_candidates()]))),
        screenshot_png: b"png".to_vec(),
    };
    let outcome = execute_vision_extract(
        &browser,
        fake_vision(VisionProposal {
            confidence: 0.9,
            action: VisionAction::ExtractFromCandidate { index: 2 },
        }),
    )
    .await;

    assert_vision_assist_failed(&outcome);
}

#[tokio::test]
async fn successful_candidate_extraction_records_index_without_runtime_value() {
    let secret = "runtime-extracted-secret-4f91";
    let metrics = OperationalMetrics::default();
    let browser = FakeBrowser {
        candidate_responses: Arc::new(Mutex::new(VecDeque::from([vec![candidate_with_role(
            "link",
            "Account",
            secret,
            BTreeMap::new(),
        )]]))),
        screenshot_png: b"png".to_vec(),
    };
    let dir = tempfile::tempdir().unwrap();
    let outcome = IntentEngine::execute(
        &extract(vec![field("account", "Account", ExtractValueKind::Text)]),
        &PageId::new(),
        &browser,
        &VisionContext {
            session_ok: true,
            capability_ok: true,
            assist: Some(metric_vision(
                VisionProposal {
                    confidence: 0.9,
                    action: VisionAction::ExtractFromCandidate { index: 0 },
                },
                metrics.clone(),
            )),
            corpus: Some(VisionCorpus::new(dir.path()).unwrap()),
            context_store: None,
            ..VisionContext::default()
        },
    )
    .await;
    assert!(matches!(outcome, IntentOutcome::Completed { .. }));
    let line = std::fs::read_to_string(dir.path().join("vision-corpus.jsonl")).unwrap();
    assert!(!line.contains(secret));
    let record: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(record["targetIndex"], 0);
    assert_eq!(
        record["modelResponse"]["action"],
        serde_json::json!({"kind":"extractFromCandidate","index":0})
    );
    assert_eq!(metrics.snapshot().vision.accepted, 1);
    assert_eq!(metrics.snapshot().verification.accepted, 1);
}

#[tokio::test]
async fn extract_from_candidate_without_role_or_name_reports_vision_assist_failed() {
    let role_missing = candidate("role-missing", "Account role missing", BTreeMap::new());
    let mut name_missing = candidate_with_role(
        "link",
        "name-missing",
        "Account name missing",
        BTreeMap::new(),
    );
    name_missing.name = None;
    let browser = FakeBrowser {
        candidate_responses: Arc::new(Mutex::new(VecDeque::from([vec![
            role_missing,
            name_missing,
        ]]))),
        screenshot_png: b"png".to_vec(),
    };
    let outcome = execute_vision_extract(
        &browser,
        fake_vision(VisionProposal {
            confidence: 0.9,
            action: VisionAction::ExtractFromCandidate { index: 0 },
        }),
    )
    .await;

    assert_vision_assist_failed(&outcome);
}

#[tokio::test]
async fn extract_type_into_candidate_reports_vision_assist_failed() {
    let browser = FakeBrowser {
        candidate_responses: Arc::new(Mutex::new(VecDeque::from([ambiguous_extract_candidates()]))),
        screenshot_png: b"png".to_vec(),
    };
    let outcome = execute_vision_extract(
        &browser,
        fake_vision(VisionProposal {
            confidence: 0.9,
            action: VisionAction::TypeIntoCandidate { index: 0 },
        }),
    )
    .await;

    assert_vision_assist_failed(&outcome);
}

fn ambiguous_extract_candidates() -> Vec<Candidate> {
    vec![
        candidate_with_role(
            "link",
            "Primary account",
            "Account primary",
            BTreeMap::new(),
        ),
        candidate_with_role(
            "link",
            "Secondary account",
            "Account secondary",
            BTreeMap::new(),
        ),
    ]
}

async fn execute_vision_extract(browser: &FakeBrowser, assist: Arc<FakeVision>) -> IntentOutcome {
    IntentEngine::execute(
        &extract(vec![field("account", "Account", ExtractValueKind::Text)]),
        &PageId::new(),
        browser,
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
    .await
}

fn assert_vision_assist_failed(outcome: &IntentOutcome) {
    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    let Evidence::Extraction {
        value, error_code, ..
    } = find_extraction(evidence, "account")
    else {
        unreachable!()
    };
    assert_eq!(*value, None);
    assert_eq!(*error_code, Some(ErrorCode::VisionAssistFailed));
}
