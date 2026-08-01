use chrono::{TimeZone, Utc};
use serde_json::json;
use types::{
    AttemptId, Capability, CaptureScreenshotCommand, ClickAndWaitForDownloadCommand,
    ClickAndWaitForPopupCommand, ClickCommand, ClosePageCommand, CommandClass, CommandEnvelope,
    CommandError, CommandId, CommandOutcome, CompleteFormField, CompleteFormIntent,
    CreateSessionRequest, DismissObstructionIntent, DownloadUrlCommand, ElementState, ErrorCode,
    ErrorLayer, EvaluateJavaScriptCommand, Evidence, ExecutionPath, ExecutionPolicy,
    ExecutionReason, ExecutionRecord, ExtractField, ExtractIntent, ExtractValueKind, FillIntent,
    FillValue, FollowIntent, InspectCommand, IntentCommand, IntentHints, IntentResolutionPath,
    ListPagesCommand, LocateIntent, NetworkResourceType, OpenPageCommand, PageId, PrimitiveCommand,
    RuntimeCommand, ScreenshotMode, SessionId, SubmitAndVerifyIntent, TargetSpec, TextMatch,
    TypeTextCommand, UploadFilesCommand, WaitCondition, WaitForCommand, WaitForStateIntent,
    WaitUntil, WorkflowId,
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
            },
        )),
    };
    let safe = envelope.journal_safe();
    let safe_json = serde_json::to_string(&safe).unwrap();
    assert!(!safe_json.contains("alice"));
    assert!(!safe_json.contains("signed"));
    assert!(!safe_json.contains("secret"));
    let RuntimeCommand::Primitive(PrimitiveCommand::DownloadUrl(live)) = &mut envelope.command
    else {
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

#[test]
fn checked_fill_value_round_trips_as_camel_case() {
    let value = FillValue::Checked { checked: true };
    assert_eq!(
        serde_json::to_value(&value).unwrap(),
        json!({"kind":"checked","checked":true})
    );
    assert!(matches!(
        serde_json::from_value::<FillValue>(json!({"kind":"checked","checked":false})).unwrap(),
        FillValue::Checked { checked: false }
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
        value: FillValue::Text {
            text: "a@b.co".into(),
            clear_first: true,
        },
    });
    assert_eq!(fill.class(), CommandClass::Reconciliable);

    let complete_form = IntentCommand::CompleteForm(CompleteFormIntent {
        fields: vec![CompleteFormField {
            name: "email".into(),
            purpose: "Enter email".into(),
            hints: IntentHints::default(),
            value: FillValue::Text {
                text: "a@b.co".into(),
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
        value: FillValue::Files {
            paths: vec!["./data/uploads/cv.pdf".into()],
        },
    });
    assert!(matches!(
        files,
        IntentCommand::Fill(FillIntent {
            value: FillValue::Files { .. },
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
    };
    let value = serde_json::to_value(&without).unwrap();
    assert!(value.get("excludedClasses").is_none());
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
