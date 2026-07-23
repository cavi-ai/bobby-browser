use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dom_engine::{Candidate, CandidateState};
use intent_engine::{compatible, IntentBrowser, IntentEngine, IntentOutcome, VisionContext};
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandError, ErrorCode, Evidence, FillIntent,
    FillValue, IntentCommand, IntentHints, IntentResolutionPath, PageId, TargetSpec,
    TypeTextCommand, UploadFilesCommand, WaitForCommand,
};

#[derive(Default)]
struct CallLog {
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

fn fill(purpose: &str, role: Option<&str>, value: FillValue) -> IntentCommand {
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
        &FillValue::Text {
            text: "Ada".into(),
            clear_first: true,
        },
        &textbox("Email")
    ));
    assert!(!compatible(
        &FillValue::Files {
            paths: vec!["/tmp/a.txt".into()],
        },
        &textbox("Email")
    ));
    assert!(compatible(
        &FillValue::Files {
            paths: vec!["/tmp/a.txt".into()],
        },
        &file_input("Resume")
    ));
    assert!(!compatible(
        &FillValue::Text {
            text: "Ada".into(),
            clear_first: true,
        },
        &file_input("Resume")
    ));
    assert!(compatible(
        &FillValue::Select {
            option: "CA".into(),
        },
        &combobox("State")
    ));
    assert!(!compatible(
        &FillValue::Select {
            option: "CA".into(),
        },
        &textbox("Email")
    ));
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
            FillValue::Text {
                text: "Ada".into(),
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
async fn fill_select_types_option_via_type_text() {
    // Chromium worker has no select primitive; Select uses TypeTextCommand.
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
            FillValue::Select {
                option: "CA".into(),
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
        assert_eq!(log.type_text[0].value, "CA");
    }
    assert!(evidence.iter().any(|item| matches!(
        item,
        Evidence::IntentExecution { record } if record.verification == "filled"
    )));
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
            FillValue::Files {
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
        assert_eq!(log.upload_files[0].paths, vec!["/tmp/resume.txt".to_owned()]);
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
            FillValue::Files {
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
