use chrono::{TimeZone, Utc};
use serde_json::json;
use types::{
    AttemptId, Capability, CaptureScreenshotCommand, ClickAndWaitForDownloadCommand,
    ClickAndWaitForPopupCommand, ClickCommand, ClosePageCommand, CommandClass, CommandEnvelope,
    CommandId, CommandOutcome, CreateSessionRequest, DownloadUrlCommand, ElementState, ErrorCode,
    EvaluateJavaScriptCommand, Evidence, ExecutionPath, ExecutionPolicy, ExecutionReason,
    InspectCommand, ListPagesCommand, OpenPageCommand, PageId, PrimitiveCommand, ScreenshotMode,
    SessionId, TargetSpec, TextMatch, TypeTextCommand, UploadFilesCommand, WaitCondition,
    WaitForCommand, WaitUntil, WorkflowId,
};
use uuid::Uuid;

fn uuid(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

fn test_envelope(command: PrimitiveCommand) -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: Some(PageId::new()),
        deadline: Utc::now() + chrono::Duration::minutes(1),
        command,
    }
}

#[test]
fn journal_safe_replaces_malformed_url_instead_of_persisting_secrets() {
    let envelope = test_envelope(PrimitiveCommand::Navigate(types::NavigateCommand {
        url: "https://user:password@example.test/%zz?token=top-secret#fragment".into(),
        wait_until: WaitUntil::Commit,
        timeout_ms: 1000,
    }));
    let durable = serde_json::to_string(&envelope.journal_safe()).unwrap();
    assert!(!durable.contains("password"));
    assert!(!durable.contains("top-secret"));
    assert!(!durable.contains("fragment"));
}

#[test]
fn journal_safe_outcome_redacts_all_evidence_urls() {
    let outcome = CommandOutcome::Completed {
        command_id: CommandId::new(),
        evidence: vec![
            Evidence::Navigation {
                url: "https://user:pass@example.test/page?token=secret#frag".into(),
                title: "page".into(),
            },
            Evidence::ExecutionPath {
                path: ExecutionPath::DirectHttp,
                reason: ExecutionReason::EligibleStaticDocument,
                state_version: 1,
                elapsed_ms: 2,
                bytes: Some(3),
                sha256: Some("abc".into()),
                final_url: Some("https://example.test/final?signed=secret".into()),
                content_type: Some("text/html".into()),
                status: Some(200),
                redirect_chain: vec!["https://example.test/hop?key=secret".into()],
            },
        ],
    };
    let durable = serde_json::to_string(&outcome.journal_safe()).unwrap();
    assert!(!durable.contains("user"));
    assert!(!durable.contains("pass"));
    assert!(!durable.contains("secret"));
    assert!(!durable.contains("frag"));
    assert!(durable.contains("text/html"));
    assert!(durable.contains("\"status\":200"));
}

#[test]
fn adaptive_http_download_command_is_reconciliable_and_round_trips() {
    let command = PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
        url: "https://example.test/report.bin".into(),
        expected_content_type: Some("application/octet-stream".into()),
        max_bytes: 1_048_576,
    });

    assert_eq!(command.class(), CommandClass::Reconciliable);
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
        final_url: None,
        content_type: None,
        status: None,
        redirect_chain: Vec::new(),
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
fn journal_safe_envelope_removes_all_url_secrets_without_mutating_live_command() {
    let mut envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId(uuid(1)),
        workflow_id: WorkflowId(uuid(2)),
        attempt_id: AttemptId(uuid(3)),
        session_id: SessionId(uuid(4)),
        page_id: Some(PageId(uuid(5))),
        deadline: Utc.with_ymd_and_hms(2026, 7, 16, 12, 0, 0).unwrap(),
        command: PrimitiveCommand::DownloadUrl(types::DownloadUrlCommand {
            url: "https://alice:pw@example.com/file?token=signed#secret".into(),
            expected_content_type: None,
            max_bytes: 10,
        }),
    };
    let safe = envelope.journal_safe();
    let safe_json = serde_json::to_string(&safe).unwrap();
    assert!(!safe_json.contains("alice"));
    assert!(!safe_json.contains("signed"));
    assert!(!safe_json.contains("secret"));
    let PrimitiveCommand::DownloadUrl(live) = &mut envelope.command else {
        unreachable!()
    };
    assert!(live.url.contains("token=signed"));
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

#[test]
fn browser_execution_evidence_is_journal_safe_without_paths_or_content() {
    let evidence = Evidence::BrowserExecution {
        engine: "firefox".into(),
        browser_version: "128.0".into(),
        profile_id: "847ac21e-a1f4-4a31-b35b-ff1741c480f7".into(),
        interaction_path: "engineNative".into(),
    };

    let value = serde_json::to_value(evidence.journal_safe()).unwrap();
    assert_eq!(value["kind"], "browserExecution");
    assert_eq!(value["engine"], "firefox");
    assert_eq!(value["browserVersion"], "128.0");
    assert_eq!(value["profileId"], "847ac21e-a1f4-4a31-b35b-ff1741c480f7");
    assert_eq!(value["interactionPath"], "engineNative");
    assert!(value.get("profilePath").is_none());
    assert!(value.get("value").is_none());
}

#[test]
fn evaluate_javascript_command_is_reconciliable_and_round_trips_as_camel_case() {
    let command = PrimitiveCommand::EvaluateJavaScript(EvaluateJavaScriptCommand {
        expression: "document.title".into(),
        timeout_ms: 2_000,
        await_promise: true,
    });

    assert_eq!(command.class(), CommandClass::Reconciliable);
    let value = serde_json::to_value(&command).unwrap();
    assert_eq!(value["kind"], json!("evaluateJavaScript"));
    assert_eq!(value["input"]["expression"], json!("document.title"));
    assert_eq!(value["input"]["timeoutMs"], json!(2000));
    assert_eq!(value["input"]["awaitPromise"], json!(true));
    let round_tripped: PrimitiveCommand = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(round_tripped).unwrap(), value);
}

#[test]
fn evaluate_javascript_command_defaults_await_promise_to_false_when_absent() {
    let value = json!({
        "kind": "evaluateJavaScript",
        "input": {
            "expression": "1 + 1",
            "timeoutMs": 500
        }
    });
    let command: PrimitiveCommand = serde_json::from_value(value).unwrap();
    let PrimitiveCommand::EvaluateJavaScript(command) = command else {
        panic!("expected EvaluateJavaScript variant");
    };
    assert_eq!(command.expression, "1 + 1");
    assert_eq!(command.timeout_ms, 500);
    assert!(!command.await_promise);
}

#[test]
fn javascript_result_evidence_round_trips_as_camel_case() {
    let evidence = Evidence::JavaScriptResult {
        value: json!({"answer": 42}),
        truncated: false,
    };
    let value = serde_json::to_value(&evidence).unwrap();
    assert_eq!(value["kind"], json!("javaScriptResult"));
    assert_eq!(value["value"], json!({"answer": 42}));
    assert_eq!(value["truncated"], json!(false));
    let round_tripped: Evidence = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(round_tripped).unwrap(), value);
}

#[test]
fn intent_and_vision_capabilities_round_trip() {
    assert_eq!(
        serde_json::to_string(&Capability::IntentExecute).unwrap(),
        "\"intent:execute\""
    );
    assert_eq!(
        serde_json::to_string(&Capability::VisionAssist).unwrap(),
        "\"vision:assist\""
    );
    assert_eq!(Capability::IntentExecute.as_str(), "intent:execute");
    assert_eq!(Capability::VisionAssist.as_str(), "vision:assist");
}

#[test]
fn execution_policy_defaults_deny_vision() {
    let policy = ExecutionPolicy::default();
    assert!(!policy.javascript_evaluation);
    assert!(!policy.vision_assist);
    let value = serde_json::to_value(&policy).unwrap();
    assert_eq!(value["visionAssist"], false);
    let parsed: ExecutionPolicy = serde_json::from_value(serde_json::json!({})).unwrap();
    assert_eq!(parsed, ExecutionPolicy::default());
}

#[test]
fn intent_error_codes_round_trip() {
    for code in [
        ErrorCode::IntentCompileFailed,
        ErrorCode::IntentActionMismatch,
        ErrorCode::ObstructionSuspected,
        ErrorCode::VisionAssistDenied,
        ErrorCode::VisionAssistFailed,
    ] {
        let value = serde_json::to_value(code).unwrap();
        let parsed: ErrorCode = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, code);
    }
}

#[test]
fn execution_policy_defaults_to_deny() {
    assert!(!ExecutionPolicy::default().javascript_evaluation);

    let value = serde_json::to_value(ExecutionPolicy::default()).unwrap();
    assert_eq!(
        value,
        json!({"javascriptEvaluation": false, "visionAssist": false})
    );

    let explicit_grant: ExecutionPolicy =
        serde_json::from_value(json!({"javascriptEvaluation": true})).unwrap();
    assert!(explicit_grant.javascript_evaluation);
}

#[test]
fn execution_policy_field_defaults_when_omitted_from_json() {
    let policy: ExecutionPolicy = serde_json::from_value(json!({})).unwrap();
    assert_eq!(policy, ExecutionPolicy::default());
}

#[test]
fn create_session_request_without_execution_policy_deserializes_to_default_deny() {
    let request: CreateSessionRequest = serde_json::from_value(json!({
        "profile": "default",
        "proxy": null
    }))
    .unwrap();
    assert_eq!(request.execution_policy, ExecutionPolicy::default());
    assert!(!request.execution_policy.javascript_evaluation);
}

#[test]
fn create_session_request_honors_explicit_execution_policy_grant() {
    let request: CreateSessionRequest = serde_json::from_value(json!({
        "profile": "default",
        "proxy": null,
        "executionPolicy": {"javascriptEvaluation": true}
    }))
    .unwrap();
    assert!(request.execution_policy.javascript_evaluation);
}
