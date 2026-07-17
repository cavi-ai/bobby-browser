use chrono::{TimeZone, Utc};
use serde_json::json;
use types::{
    AttemptId, CaptureScreenshotCommand, ClickAndWaitForDownloadCommand,
    ClickAndWaitForPopupCommand, ClickCommand, ClosePageCommand, CommandClass, CommandEnvelope,
    CommandId, DownloadUrlCommand, ElementState, Evidence, ExecutionPath, ExecutionReason,
    InspectCommand, ListPagesCommand, OpenPageCommand, PageId, PrimitiveCommand, ScreenshotMode,
    SessionId, TargetSpec, TextMatch, TypeTextCommand, UploadFilesCommand, WaitCondition,
    WaitForCommand, WaitUntil, WorkflowId,
};
use uuid::Uuid;

fn uuid(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

#[test]
fn adaptive_http_download_command_is_replayable_and_round_trips() {
    let command = PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
        url: "https://example.test/report.bin".into(),
        expected_content_type: Some("application/octet-stream".into()),
        max_bytes: 1_048_576,
    });

    assert_eq!(command.class(), CommandClass::Replayable);
    let value = serde_json::to_value(&command).unwrap();
    assert_eq!(value["kind"], "downloadUrl");
    let round_tripped: PrimitiveCommand = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(round_tripped).unwrap(), value);
}

#[test]
fn adaptive_http_execution_path_evidence_is_stable_and_round_trips() {
    let evidence = Evidence::ExecutionPath {
        path: ExecutionPath::ChromiumFallback,
        reason: ExecutionReason::JavascriptRequired,
        state_version: 7,
        elapsed_ms: 12,
        bytes: Some(128),
        sha256: Some("abc".into()),
    };

    let value = serde_json::to_value(&evidence).unwrap();
    assert_eq!(value["path"], "chromiumFallback");
    let round_tripped: Evidence = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(round_tripped).unwrap(), value);
}

#[test]
fn adaptive_http_failures_have_stable_error_codes() {
    let cases = [
        (types::ErrorCode::NetworkPolicyDenied, "networkPolicyDenied"),
        (
            types::ErrorCode::HttpResponseTooLarge,
            "httpResponseTooLarge",
        ),
        (types::ErrorCode::HttpTransferFailed, "httpTransferFailed"),
        (types::ErrorCode::HttpStateConflict, "httpStateConflict"),
        (
            types::ErrorCode::HttpEquivalenceUnproven,
            "httpEquivalenceUnproven",
        ),
    ];

    for (code, expected) in cases {
        let value = serde_json::to_value(code).unwrap();
        assert_eq!(value, json!(expected));
        let round_tripped: types::ErrorCode = serde_json::from_value(value).unwrap();
        assert_eq!(round_tripped, code);
    }
}

#[test]
fn semantic_target_wait_and_screenshot_contracts_are_stable() {
    let target = TargetSpec {
        role: Some("button".into()),
        accessible_name: Some("Continue".into()),
        label: None,
        text: Some(TextMatch::Exact("Continue".into())),
        test_id: Some("continue".into()),
        css: None,
        attributes: Default::default(),
        frame_path: Vec::new(),
        shadow_path: Vec::new(),
        ordinal: None,
        allow_best_match: false,
    };
    let wait = PrimitiveCommand::WaitFor(WaitForCommand {
        condition: WaitCondition::Element {
            target: Box::new(target.clone()),
            state: ElementState::Visible,
        },
        timeout_ms: 5_000,
    });
    let screenshot = PrimitiveCommand::CaptureScreenshot(CaptureScreenshotCommand {
        mode: ScreenshotMode::Element {
            target: Box::new(target),
        },
    });
    let click = PrimitiveCommand::Click(ClickCommand {
        selector: String::new(),
        target: Some(target_spec("button", "Continue")),
        boundary: false,
        expected_url: None,
    });

    assert_eq!(wait.class(), CommandClass::Replayable);
    assert_eq!(screenshot.class(), CommandClass::Replayable);
    assert_eq!(
        serde_json::to_value(click).unwrap()["input"]["target"]["role"],
        json!("button")
    );
    assert_eq!(
        serde_json::to_value(wait).unwrap()["kind"],
        json!("waitFor")
    );
    assert_eq!(
        serde_json::to_value(screenshot).unwrap()["kind"],
        json!("captureScreenshot")
    );
}

fn target_spec(role: &str, name: &str) -> TargetSpec {
    TargetSpec {
        role: Some(role.into()),
        accessible_name: Some(name.into()),
        ..TargetSpec::default()
    }
}

#[test]
fn new_target_failures_have_stable_error_codes() {
    assert_eq!(
        serde_json::to_value(types::ErrorCode::TargetAmbiguous).unwrap(),
        json!("targetAmbiguous")
    );
    assert_eq!(
        serde_json::to_value(types::ErrorCode::WaitConditionTimedOut).unwrap(),
        json!("waitConditionTimedOut")
    );
    assert_eq!(
        serde_json::to_value(types::ErrorCode::ScreenshotCaptureFailed).unwrap(),
        json!("screenshotCaptureFailed")
    );
}

#[test]
fn resolution_wait_and_screenshot_evidence_are_typed() {
    let evidence = types::Evidence::Screenshot {
        artifact_id: "artifact-1".into(),
        media_type: "image/png".into(),
        width: 800,
        height: 600,
        bytes: 42,
        sha256: "abc".into(),
    };
    assert_eq!(
        serde_json::to_value(evidence).unwrap()["kind"],
        json!("screenshot")
    );
    let candidate = types::CandidateEvidence {
        role: Some("button".into()),
        name: Some("Continue".into()),
        score: 100,
        reasons: vec!["exactAccessibleName".into()],
    };
    assert_eq!(
        serde_json::to_value(candidate).unwrap()["score"],
        json!(100)
    );
}

#[test]
fn command_envelope_uses_stable_camel_case_json() {
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId(uuid(1)),
        workflow_id: WorkflowId(uuid(2)),
        attempt_id: AttemptId(uuid(3)),
        session_id: SessionId(uuid(4)),
        page_id: Some(PageId(uuid(5))),
        deadline: Utc.with_ymd_and_hms(2026, 7, 16, 12, 0, 0).unwrap(),
        command: PrimitiveCommand::Navigate(types::NavigateCommand {
            url: "https://example.com".into(),
            wait_until: WaitUntil::Interactive,
            timeout_ms: 30_000,
        }),
    };

    let value = serde_json::to_value(envelope).unwrap();
    assert_eq!(value["schemaVersion"], json!(1));
    assert_eq!(value["commandId"], json!(uuid(1)));
    assert_eq!(value["sessionId"], json!(uuid(4)));
    assert_eq!(value["pageId"], json!(uuid(5)));
    assert_eq!(value["command"]["kind"], json!("navigate"));
    assert_eq!(value["command"]["input"]["waitUntil"], json!("interactive"));
}

#[test]
fn commands_expose_recovery_class() {
    assert_eq!(
        PrimitiveCommand::Inspect(InspectCommand::default()).class(),
        CommandClass::Replayable
    );
    assert_eq!(
        PrimitiveCommand::TypeText(TypeTextCommand {
            selector: "#name".into(),
            target: None,
            value: "Ada".into(),
            clear_first: true,
        })
        .class(),
        CommandClass::Reconciliable
    );
    assert_eq!(
        PrimitiveCommand::Click(ClickCommand {
            selector: "#submit".into(),
            target: None,
            boundary: true,
            expected_url: None,
        })
        .class(),
        CommandClass::Boundary
    );
}

#[test]
fn workflow_io_commands_have_stable_json_and_recovery_classes() {
    let cases = [
        (
            PrimitiveCommand::UploadFiles(UploadFilesCommand {
                selector: "#resume".into(),
                target: None,
                paths: vec!["/uploads/resume.pdf".into()],
            }),
            "uploadFiles",
            CommandClass::Reconciliable,
        ),
        (
            PrimitiveCommand::OpenPage(OpenPageCommand {
                url: Some("https://example.com".into()),
            }),
            "openPage",
            CommandClass::Replayable,
        ),
        (
            PrimitiveCommand::ListPages(ListPagesCommand),
            "listPages",
            CommandClass::Replayable,
        ),
        (
            PrimitiveCommand::ClosePage(ClosePageCommand {
                page_id: PageId(uuid(9)),
            }),
            "closePage",
            CommandClass::Reconciliable,
        ),
        (
            PrimitiveCommand::ClickAndWaitForPopup(ClickAndWaitForPopupCommand {
                selector: "#popup".into(),
                target: None,
                timeout_ms: 5_000,
            }),
            "clickAndWaitForPopup",
            CommandClass::Boundary,
        ),
        (
            PrimitiveCommand::ClickAndWaitForDownload(ClickAndWaitForDownloadCommand {
                selector: "#download".into(),
                target: None,
                timeout_ms: 5_000,
            }),
            "clickAndWaitForDownload",
            CommandClass::Boundary,
        ),
    ];

    for (command, kind, class) in cases {
        assert_eq!(command.class(), class);
        assert_eq!(serde_json::to_value(command).unwrap()["kind"], json!(kind));
    }
}

#[test]
fn workflow_evidence_is_typed_and_camel_case() {
    let evidence = Evidence::Download {
        filename: "fixture.bin".into(),
        path: "/downloads/session/fixture.bin".into(),
        bytes: 4,
        sha256: "9f64a747".into(),
    };
    let value = serde_json::to_value(evidence).unwrap();
    assert_eq!(value["kind"], json!("download"));
    assert_eq!(value["sha256"], json!("9f64a747"));
    assert_eq!(value["bytes"], json!(4));
}
