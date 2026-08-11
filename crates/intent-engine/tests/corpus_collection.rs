use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use dom_engine::{Candidate, CandidateState};
use intent_engine::{
    IntentBrowser, IntentEngine, IntentOutcome, VisionAction, VisionAssist, VisionContext,
    VisionCorpus, VisionProposal, VisionProposeRequest,
};
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandError, ErrorCode, Evidence, IntentCommand,
    IntentHints, LocateIntent, PageId, TargetSpec, TypeTextCommand, UploadFilesCommand,
    WaitForCommand,
};

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

#[derive(Default)]
struct FakeBrowser {
    candidates: Vec<Candidate>,
    resolved_at_point: Option<(String, String)>,
    element_at_point_called: Arc<AtomicBool>,
}

#[async_trait]
impl IntentBrowser for FakeBrowser {
    async fn collect_candidates(
        &self,
        _page_id: &PageId,
        _target: &TargetSpec,
    ) -> Result<Vec<Candidate>, CommandError> {
        Ok(self.candidates.clone())
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
        Ok(vec![])
    }

    async fn element_at_point(
        &self,
        _page_id: &PageId,
        _x: f64,
        _y: f64,
    ) -> Result<Option<(String, String)>, CommandError> {
        self.element_at_point_called.store(true, Ordering::SeqCst);
        Ok(self.resolved_at_point.clone())
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
        Ok((b"png-bytes".to_vec(), vec![]))
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

fn candidate(role: &str, name: &str) -> Candidate {
    Candidate {
        id: name.into(),
        css: Some(format!("[aria-label='{name}']")),
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

/// The locate asks for a link; the page only has buttons, so resolution
/// fails with candidates present and the escalation carries them.
fn unresolvable_locate() -> IntentCommand {
    IntentCommand::Locate(LocateIntent {
        purpose: "Continue to checkout".into(),
        hints: IntentHints {
            role: Some("link".into()),
            ..IntentHints::default()
        },
    })
}

fn read_records(dir: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(dir.join("vision-corpus.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("valid corpus jsonl"))
        .collect()
}

#[tokio::test]
async fn candidate_grounded_corpus_actions_are_index_only() {
    for action in [
        VisionAction::TypeIntoCandidate { index: 1 },
        VisionAction::ExtractFromCandidate { index: 1 },
    ] {
        let assist = Arc::new(FakeVision {
            proposal: VisionProposal {
                confidence: 0.10,
                action,
            },
        });
        let dir = tempfile::tempdir().unwrap();
        let corpus = VisionCorpus::new(dir.path()).unwrap();
        let outcome = IntentEngine::execute(
            &unresolvable_locate(),
            &PageId::new(),
            &FakeBrowser {
                candidates: vec![
                    candidate("button", "Continue"),
                    candidate("button", "Cancel"),
                ],
                resolved_at_point: None,
                element_at_point_called: Arc::new(AtomicBool::new(false)),
            },
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
        assert!(matches!(outcome, IntentOutcome::Failed { .. }));

        let action = &read_records(dir.path())[0]["modelResponse"]["action"];
        assert!(matches!(
            action["kind"].as_str(),
            Some("typeIntoCandidate" | "extractFromCandidate")
        ));
        assert_eq!(action["index"], 1);
        assert!(action.get("text").is_none());
        assert!(action.get("value").is_none());
    }
}

#[tokio::test]
async fn completed_escalation_writes_a_corpus_record_with_target_index() {
    let assist = Arc::new(FakeVision {
        proposal: VisionProposal {
            confidence: 0.91,
            action: VisionAction::Click { x: 12.0, y: 34.0 },
        },
    });
    let element_at_point_called = Arc::new(AtomicBool::new(false));
    let browser = FakeBrowser {
        candidates: vec![
            candidate("button", "Continue"),
            candidate("button", "Cancel"),
        ],
        resolved_at_point: Some(("button".into(), "Continue".into())),
        element_at_point_called: element_at_point_called.clone(),
    };
    let page_id = PageId::new();
    let dir = tempfile::tempdir().unwrap();
    let corpus = VisionCorpus::new(dir.path()).unwrap();

    let outcome = IntentEngine::execute(
        &unresolvable_locate(),
        &page_id,
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

    let IntentOutcome::Completed { .. } = outcome else {
        panic!("expected Completed, got {outcome:?}");
    };
    assert!(element_at_point_called.load(Ordering::SeqCst));

    let records = read_records(dir.path());
    assert_eq!(records.len(), 1, "one corpus record expected");
    let record = &records[0];
    assert_eq!(record["success"], true);
    assert_eq!(record["outcomeStage"], "visionFallback");
    assert_eq!(record["targetIndex"], 0);
    assert_eq!(record["resolvedElement"]["role"], "button");
    assert_eq!(record["resolvedElement"]["name"], "Continue");
    assert_eq!(record["stuck"], "targetMissing");
    assert_eq!(record["purpose"], "Continue to checkout");
    assert_eq!(record["modelResponse"]["action"]["kind"], "click");
    let candidates = record["contextCandidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0]["name"], "Continue");
    assert!(!record["imageB64"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn floor_rejection_writes_a_failed_record_without_resolution() {
    let assist = Arc::new(FakeVision {
        proposal: VisionProposal {
            confidence: 0.10,
            action: VisionAction::Click { x: 12.0, y: 34.0 },
        },
    });
    let element_at_point_called = Arc::new(AtomicBool::new(false));
    let browser = FakeBrowser {
        candidates: vec![candidate("button", "Continue")],
        resolved_at_point: None,
        element_at_point_called: element_at_point_called.clone(),
    };
    let page_id = PageId::new();
    let dir = tempfile::tempdir().unwrap();
    let corpus = VisionCorpus::new(dir.path()).unwrap();

    let outcome = IntentEngine::execute(
        &unresolvable_locate(),
        &page_id,
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

    let IntentOutcome::Failed { .. } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert!(
        !element_at_point_called.load(Ordering::SeqCst),
        "resolution must not run for an unexecuted proposal"
    );

    let records = read_records(dir.path());
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record["success"], false);
    assert_eq!(record["outcomeStage"], "visionRejectionFloor");
    assert!(record.get("targetIndex").is_none() || record["targetIndex"].is_null());
}

#[tokio::test]
async fn no_corpus_configured_writes_nothing() {
    let assist = Arc::new(FakeVision {
        proposal: VisionProposal {
            confidence: 0.91,
            action: VisionAction::Click { x: 12.0, y: 34.0 },
        },
    });
    let browser = FakeBrowser {
        candidates: vec![candidate("button", "Continue")],
        resolved_at_point: Some(("button".into(), "Continue".into())),
        element_at_point_called: Arc::new(AtomicBool::new(false)),
    };
    let page_id = PageId::new();

    let outcome = IntentEngine::execute(
        &unresolvable_locate(),
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
    // Nothing to assert on disk: the sink was never constructed. Reaching
    // Completed without a corpus is the regression guard for the
    // byte-identical default path.
    assert!(matches!(outcome, IntentOutcome::Completed { .. }));
}
