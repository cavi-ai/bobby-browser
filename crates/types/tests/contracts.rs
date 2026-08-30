use chrono::{TimeZone, Utc};
use serde_json::json;
use types::{
    AttemptId, Capability, CaptureScreenshotCommand, ClickAndWaitForDownloadCommand,
    ClickAndWaitForPopupCommand, ClickCommand, ClickModifier, ClosePageCommand, CommandClass,
    CommandEnvelope, CommandError, CommandId, CommandOutcome, CompleteFormField,
    CompleteFormIntent, ControlAction, ControlActionCommand, ControlActionEvidence,
    CreateSessionRequest, DismissObstructionIntent, DownloadUrlCommand, ElementState, ErrorCode,
    ErrorLayer, EvaluateJavaScriptCommand, Evidence, ExecutionPath, ExecutionPolicy,
    ExecutionReason, ExecutionRecord, ExtractField, ExtractIntent, ExtractValueKind, FillIntent,
    FollowIntent, FormControlOperation, FormControlState, FormControlTarget, FormControlValidity,
    InspectCommand, IntentCommand, IntentHints, IntentResolutionPath, ListPagesCommand,
    LocateIntent, NetworkResourceType, OpenPageCommand, PageId, PrimitiveCommand, RuntimeCommand,
    ScreenshotMode, SessionId, SubmitAndVerifyIntent, TargetSpec, TextMatch, TypeTextCommand,
    UploadFilesCommand, WaitCondition, WaitForCommand, WaitForStateIntent, WaitUntil, WorkflowId,
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
        command: RuntimeCommand::Primitive(command),
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
        save_as: None,
    });

    assert_eq!(command.class(), CommandClass::Reconciliable);
    let value = serde_json::to_value(&command).unwrap();
    assert_eq!(value["kind"], "downloadUrl");
    let round_tripped: PrimitiveCommand = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(round_tripped).unwrap(), value);
}

#[test]
fn adaptive_http_download_preserves_requested_save_path() {
    let value = json!({
        "kind": "downloadUrl",
        "input": {
            "url": "https://example.test/report.csv",
            "expectedContentType": "text/csv",
            "maxBytes": 1_048_576,
            "saveAs": "/allowed/downloads/report.csv"
        }
    });

    let command: PrimitiveCommand = serde_json::from_value(value.clone()).unwrap();

    assert_eq!(serde_json::to_value(command).unwrap(), value);
}

#[test]
fn adaptive_http_execution_path_evidence_is_stable_and_round_trips() {
    let evidence = Evidence::ExecutionPath {
        path: ExecutionPath::BrowserFallback,
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
    assert_eq!(value["path"], "browserFallback");
    let round_tripped: Evidence = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(round_tripped).unwrap(), value);
}

/// Journals recorded before the rename still replay: the engine-shaped names
/// deserialize onto the strategy-shaped variants.
#[test]
fn legacy_chromium_execution_path_names_still_deserialize() {
    for (legacy, expected) in [
        ("chromium", ExecutionPath::Browser),
        ("chromiumFallback", ExecutionPath::BrowserFallback),
    ] {
        let path: ExecutionPath = serde_json::from_value(serde_json::json!(legacy)).unwrap();
        assert_eq!(path, expected);
    }
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
fn click_modifiers_round_trip_and_legacy_clicks_default_empty() {
    let value = json!({
        "kind": "click",
        "input": {
            "selector": "#range-end",
            "target": null,
            "boundary": false,
            "expectedUrl": null,
            "modifiers": ["shift", "ctrl", "alt", "meta"]
        }
    });

    let command: PrimitiveCommand = serde_json::from_value(value.clone()).unwrap();
    let PrimitiveCommand::Click(click) = &command else {
        panic!("expected click command");
    };
    assert_eq!(
        click.modifiers,
        vec![
            ClickModifier::Shift,
            ClickModifier::Ctrl,
            ClickModifier::Alt,
            ClickModifier::Meta,
        ]
    );
    assert_eq!(serde_json::to_value(command).unwrap(), value);

    let legacy: PrimitiveCommand = serde_json::from_value(json!({
        "kind": "click",
        "input": {
            "selector": "#plain",
            "target": null,
            "boundary": false,
            "expectedUrl": null
        }
    }))
    .unwrap();
    let PrimitiveCommand::Click(legacy) = legacy else {
        panic!("expected click command");
    };
    assert!(legacy.modifiers.is_empty());
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
        modifiers: Vec::new(),
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
        serde_json::to_value(types::ErrorCode::TargetObscured).unwrap(),
        json!("targetObscured")
    );
    assert_eq!(
        serde_json::to_value(types::ErrorCode::TargetOutOfBounds).unwrap(),
        json!("targetOutOfBounds")
    );
    assert_eq!(
        serde_json::to_value(types::ErrorCode::WaitConditionTimedOut).unwrap(),
        json!("waitConditionTimedOut")
    );
    assert_eq!(
        serde_json::to_value(types::ErrorCode::ScreenshotCaptureFailed).unwrap(),
        json!("screenshotCaptureFailed")
    );
    assert_eq!(
        serde_json::to_value(types::ErrorCode::ExpectedStatePreSatisfied).unwrap(),
        json!("expectedStatePreSatisfied")
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
        command: RuntimeCommand::Primitive(PrimitiveCommand::Navigate(types::NavigateCommand {
            url: "https://example.com".into(),
            wait_until: WaitUntil::Interactive,
            timeout_ms: 30_000,
        })),
    };

    let value = serde_json::to_value(envelope).unwrap();
    assert_eq!(value["schemaVersion"], json!(2));
    assert_eq!(value["commandId"], json!(uuid(1)));
    assert_eq!(value["sessionId"], json!(uuid(4)));
    assert_eq!(value["pageId"], json!(uuid(5)));
    assert_eq!(value["command"]["kind"], json!("primitive"));
    assert_eq!(value["command"]["input"]["kind"], json!("navigate"));
    assert_eq!(
        value["command"]["input"]["input"]["waitUntil"],
        json!("interactive")
    );
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
        command: RuntimeCommand::Primitive(PrimitiveCommand::DownloadUrl(
            types::DownloadUrlCommand {
                url: "https://alice:pw@example.com/file?token=signed#secret".into(),
                expected_content_type: None,
                max_bytes: 10,
                save_as: Some("/private/downloads/report.csv".into()),
            },
        )),
    };
    let safe = envelope.journal_safe();
    let safe_json = serde_json::to_string(&safe).unwrap();
    assert!(!safe_json.contains("alice"));
    assert!(!safe_json.contains("signed"));
    assert!(!safe_json.contains("secret"));
    assert!(!safe_json.contains("/private/downloads"));
    let RuntimeCommand::Primitive(PrimitiveCommand::DownloadUrl(live)) = &mut envelope.command
    else {
        unreachable!()
    };
    assert!(live.url.contains("token=signed"));
    assert_eq!(
        live.save_as.as_deref(),
        Some("/private/downloads/report.csv")
    );
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
            expected_url: None,
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
            modifiers: Vec::new(),
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
        saved_to: Some("fixture.bin".into()),
    };
    let value = serde_json::to_value(evidence).unwrap();
    assert_eq!(value["kind"], json!("download"));
    assert_eq!(value["sha256"], json!("9f64a747"));
    assert_eq!(value["bytes"], json!(4));
    assert_eq!(value["savedTo"], json!("fixture.bin"));
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
    assert_eq!(Capability::JobSubmit.as_str(), "job:submit");
    assert_eq!(Capability::JobRead.as_str(), "job:read");
    assert_eq!(Capability::JobCancel.as_str(), "job:cancel");
    assert_eq!(
        Capability::BrowserFingerprint.as_str(),
        "browser:fingerprint"
    );
    assert_eq!(Capability::BrowserHumanize.as_str(), "browser:humanize");
    assert_eq!(
        serde_json::to_string(&Capability::BrowserFingerprint).unwrap(),
        "\"browser:fingerprint\""
    );
    assert_eq!(
        serde_json::to_string(&Capability::BrowserHumanize).unwrap(),
        "\"browser:humanize\""
    );
    assert_eq!(
        serde_json::to_string(&Capability::JobSubmit).unwrap(),
        "\"job:submit\""
    );
}

#[test]
fn every_capability_round_trips_its_wire_string_through_from_str() {
    // The workspace accepts capabilities through one FromStr table; a variant
    // added without a table entry fails here instead of at some gateway's
    // startup weeks later.
    for capability in [
        Capability::SessionRead,
        Capability::SessionWrite,
        Capability::PageRead,
        Capability::PageWrite,
        Capability::BrowserMutate,
        Capability::FileUpload,
        Capability::FileDownload,
        Capability::JavascriptEvaluate,
        Capability::IntentExecute,
        Capability::VisionAssist,
        Capability::ArtifactRead,
        Capability::ContextRead,
        Capability::ArtifactCapture,
        Capability::RecoveryRead,
        Capability::RecoveryWrite,
        Capability::JobSubmit,
        Capability::JobRead,
        Capability::JobCancel,
        Capability::AuthorityAdmin,
        Capability::BrowserFingerprint,
        Capability::BrowserHumanize,
    ] {
        let parsed: Capability = capability
            .as_str()
            .parse()
            .expect("every wire string parses back to its variant");
        assert_eq!(parsed, capability);
    }
    assert!("not:a-capability".parse::<Capability>().is_err());
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
    let default = ExecutionPolicy::default();
    assert!(!default.javascript_evaluation);
    assert!(!default.vision_assist);
    assert!(!default.fingerprint);
    assert!(!default.humanize);

    // Asserting the whole object, not just the flags read above, is what makes
    // this a deny-by-default guard rather than a spot check: a new privileged
    // flag added with any default other than `false` fails here.
    let value = serde_json::to_value(&default).unwrap();
    assert_eq!(
        value,
        json!({
            "javascriptEvaluation": false,
            "visionAssist": false,
            "fingerprint": false,
            "humanize": false
        })
    );
    for (name, flag) in value.as_object().expect("policy is an object") {
        assert_eq!(flag, &json!(false), "{name} does not default to denied");
    }

    let explicit_grant: ExecutionPolicy =
        serde_json::from_value(json!({"javascriptEvaluation": true})).unwrap();
    assert!(explicit_grant.javascript_evaluation);
    assert!(!explicit_grant.fingerprint);
    assert!(!explicit_grant.humanize);
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

#[test]
fn checked_control_action_round_trips_as_camel_case() {
    let value = ControlAction::SetChecked { checked: true };
    assert_eq!(
        serde_json::to_value(&value).unwrap(),
        json!({"kind":"setChecked","checked":true})
    );
    assert!(matches!(
        serde_json::from_value::<ControlAction>(json!({"kind":"setChecked","checked":false}))
            .unwrap(),
        ControlAction::SetChecked { checked: false }
    ));
}

#[test]
fn intent_commands_round_trip_and_classes() {
    let locate = IntentCommand::Locate(LocateIntent {
        purpose: "Continue".into(),
        hints: IntentHints::default(),
    });
    assert_eq!(locate.class(), CommandClass::Replayable);

    let fill = IntentCommand::Fill(FillIntent {
        purpose: "Email".into(),
        hints: IntentHints::default(),
        value: ControlAction::SetText {
            value: "a@b.co".into(),
            clear_first: true,
        },
    });
    assert_eq!(fill.class(), CommandClass::Reconciliable);

    let complete_form = IntentCommand::CompleteForm(CompleteFormIntent {
        purpose: "Complete application form".into(),
        fields: vec![CompleteFormField {
            name: "email".into(),
            purpose: "Enter email".into(),
            hints: IntentHints::default(),
            value: ControlAction::SetText {
                value: "a@b.co".into(),
                clear_first: true,
            },
        }],
    });
    assert_eq!(complete_form.class(), CommandClass::Reconciliable);
    let value = serde_json::to_value(&complete_form).unwrap();
    assert_eq!(value["kind"], "completeForm");
    assert_eq!(value["input"]["fields"][0]["name"], "email");
    let _: IntentCommand = serde_json::from_value(value).unwrap();

    let files = IntentCommand::Fill(FillIntent {
        purpose: "Resume".into(),
        hints: IntentHints::default(),
        value: ControlAction::SetFiles {
            paths: vec!["./data/uploads/cv.pdf".into()],
        },
    });
    assert!(matches!(
        files,
        IntentCommand::Fill(FillIntent {
            value: ControlAction::SetFiles { .. },
            ..
        })
    ));

    let submit = IntentCommand::SubmitAndVerify(SubmitAndVerifyIntent {
        purpose: "Submit application".into(),
        hints: IntentHints::default(),
        expected_state: WaitForCommand {
            condition: WaitCondition::Url {
                matcher: TextMatch::Contains("/thanks".into()),
            },
            timeout_ms: 5_000,
        },
    });
    assert_eq!(submit.class(), CommandClass::Boundary);

    let wait = IntentCommand::WaitForState(WaitForStateIntent {
        condition: WaitCondition::Document {
            ready: WaitUntil::Interactive,
        },
        timeout_ms: 5_000,
    });
    assert_eq!(wait.class(), CommandClass::Replayable);

    let value = serde_json::to_value(&locate).unwrap();
    assert_eq!(value["kind"], "locate");
    let _: IntentCommand = serde_json::from_value(value).unwrap();
}

#[test]
fn follow_intent_class_is_driven_by_the_caller_supplied_boundary_flag() {
    let plain_follow = IntentCommand::Follow(FollowIntent {
        purpose: "Details".into(),
        hints: IntentHints::default(),
        expected_destination: WaitForCommand {
            condition: WaitCondition::Url {
                matcher: TextMatch::Contains("/details".into()),
            },
            timeout_ms: 5_000,
        },
        boundary: false,
    });
    assert_eq!(plain_follow.class(), CommandClass::Reconciliable);

    let boundary_follow = IntentCommand::Follow(FollowIntent {
        purpose: "Sign out".into(),
        hints: IntentHints::default(),
        expected_destination: WaitForCommand {
            condition: WaitCondition::Url {
                matcher: TextMatch::Contains("/signed-out".into()),
            },
            timeout_ms: 5_000,
        },
        boundary: true,
    });
    assert_eq!(boundary_follow.class(), CommandClass::Boundary);

    let value = serde_json::to_value(&boundary_follow).unwrap();
    assert_eq!(value["kind"], "follow");
    assert_eq!(value["input"]["boundary"], true);
    let round: IntentCommand = serde_json::from_value(value).unwrap();
    assert_eq!(round.class(), CommandClass::Boundary);
}

/// Golden wire shape for agents / TypeScript SDK: nested RuntimeCommand -> Intent -> Follow.
#[test]
fn follow_runtime_command_envelope_golden_json() {
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId(uuid(1)),
        workflow_id: WorkflowId(uuid(2)),
        attempt_id: AttemptId(uuid(3)),
        session_id: SessionId(uuid(4)),
        page_id: Some(PageId(uuid(5))),
        deadline: Utc.with_ymd_and_hms(2026, 7, 16, 12, 0, 0).unwrap(),
        command: RuntimeCommand::Intent(IntentCommand::Follow(FollowIntent {
            purpose: "Details".into(),
            hints: IntentHints::default(),
            expected_destination: WaitForCommand {
                condition: WaitCondition::Url {
                    matcher: TextMatch::Contains("/details".into()),
                },
                timeout_ms: 5_000,
            },
            boundary: false,
        })),
    };

    let value = serde_json::to_value(&envelope).unwrap();
    assert_eq!(
        value,
        json!({
            "schemaVersion": 2,
            "commandId": uuid(1),
            "workflowId": uuid(2),
            "attemptId": uuid(3),
            "sessionId": uuid(4),
            "pageId": uuid(5),
            "deadline": "2026-07-16T12:00:00Z",
            "command": {
                "kind": "intent",
                "input": {
                    "kind": "follow",
                    "input": {
                        "purpose": "Details",
                        "hints": {
                            "role": null,
                            "accessibleName": null,
                            "nearText": null,
                            "ordinal": null,
                            "framePath": [],
                            "shadowPath": [],
                            "allowBestMatch": false
                        },
                        "expectedDestination": {
                            "condition": {
                                "kind": "url",
                                "matcher": {"kind": "contains", "value": "/details"}
                            },
                            "timeoutMs": 5000
                        },
                        "boundary": false
                    }
                }
            }
        })
    );
    let round: CommandEnvelope = serde_json::from_value(value).unwrap();
    assert!(matches!(
        round.command,
        RuntimeCommand::Intent(IntentCommand::Follow(_))
    ));
}

#[test]
fn dismiss_obstruction_intent_is_always_reconciliable() {
    let dismiss = IntentCommand::DismissObstruction(DismissObstructionIntent {
        purpose: "Cookie notice close button".into(),
        hints: IntentHints::default(),
        timeout_ms: 5_000,
    });
    assert_eq!(dismiss.class(), CommandClass::Reconciliable);

    let value = serde_json::to_value(&dismiss).unwrap();
    assert_eq!(value["kind"], "dismissObstruction");
    let round: IntentCommand = serde_json::from_value(value).unwrap();
    assert_eq!(round.class(), CommandClass::Reconciliable);
}

#[test]
fn dismiss_obstruction_intent_timeout_ms_defaults_when_omitted() {
    let value = json!({
        "purpose": "Cookie notice close button",
        "hints": {
            "role": null,
            "accessibleName": null,
            "nearText": null,
            "ordinal": null,
            "framePath": [],
            "shadowPath": [],
            "allowBestMatch": false
        }
    });
    let intent: DismissObstructionIntent = serde_json::from_value(value).unwrap();
    assert_eq!(
        intent.timeout_ms,
        types::DEFAULT_DISMISS_OBSTRUCTION_TIMEOUT_MS
    );
}

/// Golden wire shape for agents / TypeScript SDK: nested RuntimeCommand -> Intent -> DismissObstruction.
#[test]
fn dismiss_obstruction_runtime_command_envelope_golden_json() {
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId(uuid(1)),
        workflow_id: WorkflowId(uuid(2)),
        attempt_id: AttemptId(uuid(3)),
        session_id: SessionId(uuid(4)),
        page_id: Some(PageId(uuid(5))),
        deadline: Utc.with_ymd_and_hms(2026, 7, 16, 12, 0, 0).unwrap(),
        command: RuntimeCommand::Intent(IntentCommand::DismissObstruction(
            DismissObstructionIntent {
                purpose: "Cookie notice close button".into(),
                hints: IntentHints::default(),
                timeout_ms: 5_000,
            },
        )),
    };

    let value = serde_json::to_value(&envelope).unwrap();
    assert_eq!(
        value,
        json!({
            "schemaVersion": 2,
            "commandId": uuid(1),
            "workflowId": uuid(2),
            "attemptId": uuid(3),
            "sessionId": uuid(4),
            "pageId": uuid(5),
            "deadline": "2026-07-16T12:00:00Z",
            "command": {
                "kind": "intent",
                "input": {
                    "kind": "dismissObstruction",
                    "input": {
                        "purpose": "Cookie notice close button",
                        "hints": {
                            "role": null,
                            "accessibleName": null,
                            "nearText": null,
                            "ordinal": null,
                            "framePath": [],
                            "shadowPath": [],
                            "allowBestMatch": false
                        },
                        "timeoutMs": 5000
                    }
                }
            }
        })
    );
    let round: CommandEnvelope = serde_json::from_value(value).unwrap();
    assert!(matches!(
        round.command,
        RuntimeCommand::Intent(IntentCommand::DismissObstruction(_))
    ));
}

#[test]
fn extract_intent_is_always_replayable_and_round_trips() {
    let extract = IntentCommand::Extract(ExtractIntent {
        purpose: "Profile summary".into(),
        fields: vec![
            ExtractField {
                name: "displayName".into(),
                purpose: "Display name".into(),
                hints: IntentHints::default(),
                value: ExtractValueKind::Text,
            },
            ExtractField {
                name: "profileLink".into(),
                purpose: "Profile link".into(),
                hints: IntentHints {
                    role: Some("link".into()),
                    ..IntentHints::default()
                },
                value: ExtractValueKind::Href,
            },
            ExtractField {
                name: "userId".into(),
                purpose: "Profile link".into(),
                hints: IntentHints::default(),
                value: ExtractValueKind::Attribute {
                    attribute: "data-user-id".into(),
                },
            },
        ],
    });
    assert_eq!(extract.class(), CommandClass::Replayable);

    let value = serde_json::to_value(&extract).unwrap();
    assert_eq!(value["kind"], "extract");
    assert_eq!(value["input"]["fields"][0]["name"], "displayName");
    assert_eq!(value["input"]["fields"][0]["value"]["kind"], "text");
    assert_eq!(value["input"]["fields"][1]["value"]["kind"], "href");
    assert_eq!(value["input"]["fields"][2]["value"]["kind"], "attribute");
    assert_eq!(
        value["input"]["fields"][2]["value"]["attribute"],
        "data-user-id"
    );
    let round: IntentCommand = serde_json::from_value(value).unwrap();
    assert_eq!(round.class(), CommandClass::Replayable);
}

/// Golden wire shape for agents / TypeScript SDK: nested RuntimeCommand -> Intent -> Extract.
#[test]
fn extract_runtime_command_envelope_golden_json() {
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId(uuid(1)),
        workflow_id: WorkflowId(uuid(2)),
        attempt_id: AttemptId(uuid(3)),
        session_id: SessionId(uuid(4)),
        page_id: Some(PageId(uuid(5))),
        deadline: Utc.with_ymd_and_hms(2026, 7, 16, 12, 0, 0).unwrap(),
        command: RuntimeCommand::Intent(IntentCommand::Extract(ExtractIntent {
            purpose: "Profile summary".into(),
            fields: vec![ExtractField {
                name: "displayName".into(),
                purpose: "Display name".into(),
                hints: IntentHints::default(),
                value: ExtractValueKind::Text,
            }],
        })),
    };

    let value = serde_json::to_value(&envelope).unwrap();
    assert_eq!(
        value,
        json!({
            "schemaVersion": 2,
            "commandId": uuid(1),
            "workflowId": uuid(2),
            "attemptId": uuid(3),
            "sessionId": uuid(4),
            "pageId": uuid(5),
            "deadline": "2026-07-16T12:00:00Z",
            "command": {
                "kind": "intent",
                "input": {
                    "kind": "extract",
                    "input": {
                        "purpose": "Profile summary",
                        "fields": [{
                            "name": "displayName",
                            "purpose": "Display name",
                            "hints": {
                                "role": null,
                                "accessibleName": null,
                                "nearText": null,
                                "ordinal": null,
                                "framePath": [],
                                "shadowPath": [],
                                "allowBestMatch": false
                            },
                            "value": {"kind": "text"}
                        }]
                    }
                }
            }
        })
    );
    let round: CommandEnvelope = serde_json::from_value(value).unwrap();
    assert!(matches!(
        round.command,
        RuntimeCommand::Intent(IntentCommand::Extract(_))
    ));
}

#[test]
fn intent_resolution_paths_round_trip_their_wire_strings() {
    for (path, wire) in [
        (IntentResolutionPath::Deterministic, "deterministic"),
        (IntentResolutionPath::VisionFallback, "visionFallback"),
        (IntentResolutionPath::VisionPrefill, "visionPrefill"),
    ] {
        assert_eq!(serde_json::to_string(&path).unwrap(), format!("\"{wire}\""));
        let parsed: IntentResolutionPath = serde_json::from_str(&format!("\"{wire}\"")).unwrap();
        assert_eq!(parsed, path);
    }
}

#[test]
fn extraction_evidence_round_trips_with_and_without_a_value() {
    let resolved = Evidence::Extraction {
        field: "displayName".into(),
        value: Some("Ada Lovelace".into()),
        resolution_path: IntentResolutionPath::Deterministic,
        error_code: None,
    };
    let value = serde_json::to_value(&resolved).unwrap();
    assert_eq!(value["kind"], "extraction");
    assert_eq!(value["value"], "Ada Lovelace");
    assert!(value.get("errorCode").is_none());
    let round: Evidence = serde_json::from_value(value).unwrap();
    assert!(matches!(round, Evidence::Extraction { value: Some(_), .. }));

    let missing = Evidence::Extraction {
        field: "profileLink".into(),
        value: None,
        resolution_path: IntentResolutionPath::Deterministic,
        error_code: Some(ErrorCode::VisionAssistDenied),
    };
    let value = serde_json::to_value(&missing).unwrap();
    assert_eq!(value["errorCode"], "visionAssistDenied");
    assert!(value.get("value").is_none());
    let round: Evidence = serde_json::from_value(value).unwrap();
    assert!(matches!(
        round,
        Evidence::Extraction {
            value: None,
            error_code: Some(ErrorCode::VisionAssistDenied),
            ..
        }
    ));
}

#[test]
fn runtime_command_envelope_accepts_intent_and_primitive() {
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: None,
        deadline: Utc::now() + chrono::Duration::seconds(30),
        command: RuntimeCommand::Intent(IntentCommand::Locate(LocateIntent {
            purpose: "Search".into(),
            hints: IntentHints::default(),
        })),
    };
    let value = serde_json::to_value(&envelope).unwrap();
    assert_eq!(value["command"]["kind"], "intent");
    let round: CommandEnvelope = serde_json::from_value(value).unwrap();
    assert!(matches!(round.command, RuntimeCommand::Intent(_)));
}

/// Golden wire shape for agents / TypeScript SDK: nested RuntimeCommand → Intent → Locate.
#[test]
fn locate_runtime_command_envelope_golden_json() {
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId(uuid(1)),
        workflow_id: WorkflowId(uuid(2)),
        attempt_id: AttemptId(uuid(3)),
        session_id: SessionId(uuid(4)),
        page_id: Some(PageId(uuid(5))),
        deadline: Utc.with_ymd_and_hms(2026, 7, 16, 12, 0, 0).unwrap(),
        command: RuntimeCommand::Intent(IntentCommand::Locate(LocateIntent {
            purpose: "Continue".into(),
            hints: IntentHints::default(),
        })),
    };

    let value = serde_json::to_value(&envelope).unwrap();
    assert_eq!(
        value,
        json!({
            "schemaVersion": 2,
            "commandId": uuid(1),
            "workflowId": uuid(2),
            "attemptId": uuid(3),
            "sessionId": uuid(4),
            "pageId": uuid(5),
            "deadline": "2026-07-16T12:00:00Z",
            "command": {
                "kind": "intent",
                "input": {
                    "kind": "locate",
                    "input": {
                        "purpose": "Continue",
                        "hints": {
                            "role": null,
                            "accessibleName": null,
                            "nearText": null,
                            "ordinal": null,
                            "framePath": [],
                            "shadowPath": [],
                            "allowBestMatch": false
                        }
                    }
                }
            }
        })
    );
}

#[test]
fn intent_execution_evidence_round_trip() {
    let evidence = Evidence::IntentExecution {
        record: ExecutionRecord {
            intent_kind: "locate".into(),
            purpose: Some("Continue".into()),
            resolution_path: IntentResolutionPath::Deterministic,
            plan_summary: "role=button name~Continue".into(),
            candidates: vec![],
            wait_elapsed_ms: None,
            verification: "resolved".into(),
            artifact_ids: vec![],
            vision_proposal_sha256: None,
        },
    };
    let value = serde_json::to_value(&evidence).unwrap();
    assert_eq!(value["kind"], "intentExecution");
    let _: Evidence = serde_json::from_value(value).unwrap();
}

#[test]
fn failed_outcome_deserializes_missing_evidence_as_empty() {
    let legacy = serde_json::json!({
        "status": "failed",
        "commandId": "00000000-0000-4000-8000-000000000001",
        "error": {
            "code": "targetNotFound",
            "message": "targetNotFound",
            "layer": "page",
            "retryable": false
        }
    });
    let outcome: CommandOutcome = serde_json::from_value(legacy).unwrap();
    match outcome {
        CommandOutcome::Failed { evidence, .. } => assert!(evidence.is_empty()),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn failed_outcome_preserves_intent_execution_evidence() {
    let outcome = CommandOutcome::Failed {
        command_id: CommandId::new(),
        error: CommandError {
            code: ErrorCode::TargetNotFound,
            message: "targetNotFound".into(),
            layer: ErrorLayer::Page,
            retryable: false,
        },
        evidence: vec![Evidence::IntentExecution {
            record: ExecutionRecord {
                intent_kind: "locate".into(),
                purpose: Some("Continue".into()),
                resolution_path: IntentResolutionPath::Deterministic,
                plan_summary: "role=button name=Continue".into(),
                candidates: vec![],
                wait_elapsed_ms: None,
                verification: "targetNotFound".into(),
                artifact_ids: vec![],
                vision_proposal_sha256: None,
            },
        }],
    };
    let value = serde_json::to_value(&outcome).unwrap();
    assert_eq!(value["status"], "failed");
    assert_eq!(value["evidence"][0]["kind"], "intentExecution");
    let round: CommandOutcome = serde_json::from_value(value).unwrap();
    match round {
        CommandOutcome::Failed { evidence, .. } => {
            assert!(matches!(
                evidence.as_slice(),
                [Evidence::IntentExecution { .. }]
            ));
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn network_quiet_serializes_camel_case_and_accepts_snake_case_aliases() {
    let condition = WaitCondition::NetworkQuiet {
        idle_ms: 250,
        max_in_flight: 1,
        ignore_url_substrings: vec!["analytics".into()],
        ignore_resource_types: vec![NetworkResourceType::Image, NetworkResourceType::Xhr],
        ignore_long_lived: true,
    };

    let value = serde_json::to_value(&condition).unwrap();
    assert_eq!(
        value,
        json!({
            "kind": "networkQuiet",
            "idleMs": 250,
            "maxInFlight": 1,
            "ignoreUrlSubstrings": ["analytics"],
            "ignoreResourceTypes": ["Image", "XHR"],
            "ignoreLongLived": true
        })
    );

    let from_camel: WaitCondition = serde_json::from_value(value).unwrap();
    assert_eq!(from_camel, condition);

    let from_snake: WaitCondition = serde_json::from_value(json!({
        "kind": "networkQuiet",
        "idle_ms": 250,
        "max_in_flight": 1,
        "ignore_url_substrings": ["analytics"],
        "ignore_resource_types": ["Image", "XHR"],
        "ignore_long_lived": true
    }))
    .unwrap();
    assert_eq!(from_snake, condition);

    let minimal: WaitCondition = serde_json::from_value(json!({
        "kind": "networkQuiet",
        "idleMs": 50,
        "maxInFlight": 0
    }))
    .unwrap();
    assert_eq!(
        minimal,
        WaitCondition::NetworkQuiet {
            idle_ms: 50,
            max_in_flight: 0,
            ignore_url_substrings: Vec::new(),
            ignore_resource_types: Vec::new(),
            ignore_long_lived: false,
        }
    );
}

#[test]
fn wait_evidence_includes_excluded_classes_when_present() {
    let evidence = Evidence::Wait {
        condition: WaitCondition::NetworkQuiet {
            idle_ms: 50,
            max_in_flight: 0,
            ignore_url_substrings: vec!["beacon".into()],
            ignore_resource_types: Vec::new(),
            ignore_long_lived: true,
        },
        elapsed_ms: 60,
        observations: 3,
        excluded_classes: vec!["urlSubstring:beacon".into(), "websocket".into()],
        observed: None,
    };
    let value = serde_json::to_value(&evidence).unwrap();
    assert_eq!(value["kind"], "wait");
    assert_eq!(
        value["excludedClasses"],
        json!(["urlSubstring:beacon", "websocket"])
    );
    let round: Evidence = serde_json::from_value(value).unwrap();
    assert_eq!(round, evidence);

    let without = Evidence::Wait {
        condition: WaitCondition::Document {
            ready: WaitUntil::Interactive,
        },
        elapsed_ms: 1,
        observations: 1,
        excluded_classes: Vec::new(),
        observed: None,
    };
    let value = serde_json::to_value(&without).unwrap();
    assert!(value.get("excludedClasses").is_none());
    // Additive: an absent observation must not appear on the wire, so an
    // existing client parsing wait evidence sees exactly what it saw before.
    assert!(value.get("observed").is_none());
}

/// A satisfied wait reports what it read.
///
/// The poll already reads the value to decide whether it is satisfied. It was
/// discarded, so an agent verifying a submit paid a second round trip
/// snapshotting the page to learn what it had just confirmed.
#[test]
fn wait_evidence_carries_the_value_the_condition_matched_on() {
    let evidence = Evidence::Wait {
        condition: WaitCondition::Url {
            matcher: TextMatch::Contains("/confirmed".into()),
        },
        elapsed_ms: 42,
        observations: 2,
        excluded_classes: Vec::new(),
        observed: Some("https://example.com/order/confirmed".into()),
    };
    let value = serde_json::to_value(&evidence).unwrap();
    assert_eq!(value["observed"], "https://example.com/order/confirmed");
    let round: Evidence = serde_json::from_value(value).unwrap();
    assert_eq!(round, evidence);
}

#[test]
fn accessibility_snapshot_action_target_round_trips_without_dom_identifiers() {
    let page_id = PageId::new();
    let value = json!({
        "kind": "accessibilitySnapshot",
        "pageId": page_id,
        "nodes": [{
            "role": "textbox",
            "name": "Email address",
            "target": {
                "role": "textbox",
                "accessibleName": "Email address",
                "ordinal": 1
            }
        }],
        "truncated": false
    });

    let evidence: Evidence = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(evidence).unwrap(), value);
}

#[test]
fn control_action_contract_is_closed_reconciliable_and_secret_safe() {
    let target = FormControlTarget {
        role: "checkbox".into(),
        accessible_name: "Terms".into(),
        ordinal: None,
        frame_path: Vec::new(),
        shadow_path: Vec::new(),
    };
    let command = PrimitiveCommand::ControlAction(ControlActionCommand {
        target: target.clone(),
        action: ControlAction::SetChecked { checked: true },
    });
    assert_eq!(command.class(), CommandClass::Reconciliable);
    // The slim target shape omits ordinal/framePath/shadowPath when they
    // carry their default — absent means "no extra hint", not a different
    // wire contract.
    assert_eq!(
        serde_json::to_value(&command).unwrap(),
        json!({
            "kind": "controlAction",
            "input": {
                "target": {
                    "role": "checkbox",
                    "accessibleName": "Terms"
                },
                "action": {"kind": "setChecked", "checked": true}
            }
        })
    );
    assert!(serde_json::from_value::<ControlActionCommand>(json!({
        "target": target,
        "action": {"kind": "clear"},
        "selector": "#forbidden"
    }))
    .is_err());

    let unsafe_envelope = test_envelope(PrimitiveCommand::ControlAction(ControlActionCommand {
        target: FormControlTarget {
            role: "textbox".into(),
            accessible_name: "Upload".into(),
            ordinal: None,
            frame_path: Vec::new(),
            shadow_path: Vec::new(),
        },
        action: ControlAction::SetFiles {
            paths: vec!["/private/secret.txt".into()],
        },
    }));
    let journal = serde_json::to_string(&unsafe_envelope.journal_safe()).unwrap();
    assert!(!journal.contains("/private/secret.txt"));
    assert!(journal.contains("upload://input/0"));
}

#[test]
fn control_action_validates_bounds_and_evidence_round_trips() {
    assert!(ControlAction::SelectMany { values: Vec::new() }
        .validate()
        .is_err());
    assert!(ControlAction::SelectMany {
        values: vec!["one".into(), "one".into()]
    }
    .validate()
    .is_err());
    assert!(ControlAction::SetText {
        value: "x".repeat(types::MAX_FORM_VALUE_BYTES + 1),
        clear_first: true,
    }
    .validate()
    .is_err());

    let evidence = Evidence::ControlAction {
        action: ControlActionEvidence {
            operation: FormControlOperation::SetChecked,
            target: FormControlTarget {
                role: "checkbox".into(),
                accessible_name: "Terms".into(),
                ordinal: None,
                frame_path: Vec::new(),
                shadow_path: Vec::new(),
            },
            state: FormControlState::Checked { checked: true },
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
    };
    let value = serde_json::to_value(&evidence).unwrap();
    assert_eq!(value["kind"], "controlAction");
    assert_eq!(value["action"]["operation"], "setChecked");
    assert_eq!(
        serde_json::to_value(serde_json::from_value::<Evidence>(value.clone()).unwrap()).unwrap(),
        value
    );
}

// === L3 unification contract (fix/l3-unified-control-values) ===
// These tests define the unified mutation vocabulary: FillIntent and
// CompleteFormField carry ControlAction, FillValue no longer exists, and the
// control_action verbs (setText/setChecked/selectOne/selectMany/setFiles/
// clear) are the only wire spelling. Do not weaken these assertions.

#[test]
fn fill_uses_the_control_action_vocabulary() {
    let fill: FillIntent = serde_json::from_value(json!({
        "purpose": "Email",
        "value": {"kind":"setText","value":"a@b.co"}
    }))
    .unwrap();
    assert!(matches!(
        &fill.value,
        ControlAction::SetText { value, clear_first: true } if value == "a@b.co"
    ));
    assert_eq!(
        serde_json::to_value(&fill.value).unwrap(),
        json!({"kind":"setText","value":"a@b.co","clearFirst":true})
    );

    let appended: FillIntent = serde_json::from_value(json!({
        "purpose": "Email",
        "value": {"kind":"setText","value":"x","clearFirst":false}
    }))
    .unwrap();
    assert!(matches!(
        &appended.value,
        ControlAction::SetText {
            clear_first: false,
            ..
        }
    ));
}

#[test]
fn set_text_clear_first_defaults_to_replace_everywhere() {
    // control_action's current wire shape must keep parsing: setText without
    // clearFirst means replace, matching the previous hard-coded behavior.
    let action: ControlAction =
        serde_json::from_value(json!({"kind":"setText","value":"x"})).unwrap();
    assert!(matches!(
        action,
        ControlAction::SetText {
            clear_first: true,
            ..
        }
    ));
}

#[test]
fn fill_accepts_select_many_and_clear() {
    let many: FillIntent = serde_json::from_value(json!({
        "purpose": "Toppings",
        "value": {"kind":"selectMany","values":["ham","cheese"]}
    }))
    .unwrap();
    assert!(matches!(&many.value, ControlAction::SelectMany { values } if values.len() == 2));

    let clear: FillIntent = serde_json::from_value(json!({
        "purpose": "Email",
        "value": {"kind":"clear"}
    }))
    .unwrap();
    assert!(matches!(&clear.value, ControlAction::Clear));
}

#[test]
fn fill_round_trips_selection_and_files_under_the_unified_kinds() {
    for (value, expected) in [
        (
            json!({"kind":"selectOne","value":"Pro"}),
            json!({"kind":"selectOne","value":"Pro"}),
        ),
        (
            json!({"kind":"setChecked","checked":true}),
            json!({"kind":"setChecked","checked":true}),
        ),
        (
            json!({"kind":"setFiles","paths":["./data/uploads/cv.pdf"]}),
            json!({"kind":"setFiles","paths":["./data/uploads/cv.pdf"]}),
        ),
    ] {
        let fill: FillIntent = serde_json::from_value(json!({
            "purpose": "field",
            "value": value
        }))
        .unwrap();
        assert_eq!(serde_json::to_value(&fill.value).unwrap(), expected);
    }
}

#[test]
fn legacy_fill_value_shapes_are_rejected() {
    for legacy in [
        json!({"purpose":"Email","value":{"kind":"text","text":"a@b.co"}}),
        json!({"purpose":"Plan","value":{"kind":"select","option":"Pro"}}),
        json!({"purpose":"TOS","value":{"kind":"checked","checked":true}}),
        json!({"purpose":"CV","value":{"kind":"files","paths":["cv.pdf"]}}),
    ] {
        assert!(
            serde_json::from_value::<FillIntent>(legacy.clone()).is_err(),
            "legacy shape must no longer parse: {legacy}"
        );
    }
}
