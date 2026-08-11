//! Live installed-Chromium proof: intent resolution auto-descends one level
//! into iframes, so an in-frame control resolves without the caller naming
//! a framePath for content it cannot see.

use std::path::PathBuf;

use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, ServerConfig, StorageConfig};
use gauntlet_server::{ScenarioConfig, ScenarioServer};
use sdk_core::RuntimeService;
use types::{
    AttemptId, CommandEnvelope, CommandId, CommandOutcome, CreateSessionRequest, ElementState,
    FormControlKind, FormControlTarget, IntentCommand, IntentHints, LocateIntent, NavigateCommand,
    OpenPageRequest, PrimitiveCommand, RuntimeCommand, TargetSpec, UploadFilesCommand,
    WaitCondition, WaitForCommand, WaitUntil, WorkflowId,
};

fn chrome_executable() -> PathBuf {
    std::env::var("BOBBY_CHROME_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        })
}

fn target_spec(target: &FormControlTarget) -> TargetSpec {
    let segment = |segment: &types::SemanticTargetSegment| {
        Box::new(TargetSpec {
            role: Some(segment.role.clone()),
            accessible_name: Some(segment.accessible_name.clone()),
            ordinal: segment.ordinal,
            ..TargetSpec::default()
        })
    };
    TargetSpec {
        role: Some(target.role.clone()),
        accessible_name: Some(target.accessible_name.clone()),
        ordinal: target.ordinal,
        frame_path: target.frame_path.iter().map(segment).collect(),
        shadow_path: target.shadow_path.iter().map(segment).collect(),
        ..TargetSpec::default()
    }
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn intent_locate_resolves_inside_an_iframe_without_a_frame_path() {
    let server = ScenarioServer::start(ScenarioConfig::seeded("intent-frames"))
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/approved-upload.txt");
    let config = AppConfig {
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
        http: config::HttpConfig {
            allow_loopback: true,
            ..config::HttpConfig::default()
        },
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            shutdown_timeout_ms: 10_000,
        },
        browser: BrowserConfig {
            executable: Some(chrome_executable()),
            profiles_dir: root.path().join("profiles"),
            headless: true,
            max_active: 8,
            upload_roots: vec![fixture.parent().unwrap().to_path_buf()],
            downloads_dir: root.path().join("downloads"),
            artifacts_dir: root.path().join("artifacts"),
            max_artifact_bytes: 8 * 1024 * 1024,
            max_screenshot_dimension: 16_384,
            max_js_result_bytes: 64 * 1024,
            max_js_timeout_ms: 30_000,
        },
        storage: StorageConfig {
            journal_path: root.path().join("commands.jsonl"),
            checkpoints_dir: root.path().join("checkpoints"),
            authority_path: root.path().join("authority.json"),
            scheduler_journal_path: root.path().join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
        context: Default::default(),
        nodes: Default::default(),
    };
    let runtime = RuntimeService::build(&config).await.unwrap();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "intent-frames".into(),
            proxy: None,
            execution_policy: Default::default(),
        })
        .await
        .unwrap();
    let page = runtime
        .open_page(OpenPageRequest {
            session_id: session.id.clone(),
        })
        .await
        .unwrap();

    let submit_primitive = |command: PrimitiveCommand| {
        runtime.submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(command),
        })
    };
    let submit_intent = |command: IntentCommand| {
        runtime.submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Intent(command),
        })
    };

    let outcome = submit_primitive(PrimitiveCommand::Navigate(NavigateCommand {
        url: server.application_url("/customers/cus_atlas/documents"),
        wait_until: WaitUntil::Interactive,
        timeout_ms: 30_000,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    let form_snapshot = runtime
        .form_snapshot(&session.id, &page.id, None)
        .await
        .unwrap();
    let file_target = form_snapshot
        .forms
        .iter()
        .flat_map(|form| form.controls.iter())
        .chain(form_snapshot.unowned_controls.iter())
        .find(|control| control.control_kind == FormControlKind::File)
        .and_then(|control| control.target.as_ref())
        .map(target_spec)
        .expect("file input target from form snapshot");
    let outcome = submit_primitive(PrimitiveCommand::UploadFiles(UploadFilesCommand {
        selector: String::new(),
        target: Some(file_target),
        paths: vec![fixture.to_string_lossy().into_owned()],
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    let outcome = submit_primitive(PrimitiveCommand::Click(types::ClickCommand {
        selector: "form[aria-label='Upload customer document'] button".into(),
        target: None,
        boundary: false,
        expected_url: None,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    let outcome = submit_primitive(PrimitiveCommand::WaitFor(WaitForCommand {
        condition: WaitCondition::Element {
            target: Box::new(TargetSpec {
                css: Some("iframe#document-preview".into()),
                ..TargetSpec::default()
            }),
            state: ElementState::Attached,
        },
        timeout_ms: 15_000,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    let outcome = submit_primitive(PrimitiveCommand::Inspect(types::InspectCommand {
        selector: None,
        target: Some(TargetSpec {
            css: Some("body".into()),
            frame_path: vec![Box::new(TargetSpec {
                css: Some("#document-preview".into()),
                ..TargetSpec::default()
            })],
            ..TargetSpec::default()
        }),
        include_html: true,
    }))
    .await;
    let CommandOutcome::Completed { evidence, .. } = outcome else {
        panic!("iframe body inspection failed: {outcome:?}");
    };
    assert!(evidence.iter().any(|item| matches!(
        item,
        types::Evidence::Inspection { text, html, .. }
            if text.contains("Confirm document")
                && html.as_deref().is_some_and(|html| html.contains("confirm-preview"))
    )));

    // The confirm button lives inside the preview iframe. No framePath: the
    // gather must descend and resolve it anyway.
    let outcome = submit_intent(IntentCommand::Locate(LocateIntent {
        purpose: "Confirm button inside the document preview iframe".into(),
        hints: IntentHints {
            role: Some("button".into()),
            ..IntentHints::default()
        },
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "in-frame intent locate did not resolve: {outcome:?}"
    );

    // A plain (non-boundary) click into the frame must work and must not
    // kill the page target — the agent-path crash from the benchmark runs.
    let frame_button = || TargetSpec {
        css: Some("#confirm-preview".into()),
        frame_path: vec![Box::new(TargetSpec {
            css: Some("#document-preview".into()),
            ..TargetSpec::default()
        })],
        ..TargetSpec::default()
    };
    let outcome = submit_primitive(PrimitiveCommand::WaitFor(WaitForCommand {
        condition: WaitCondition::Element {
            target: Box::new(frame_button()),
            state: ElementState::Visible,
        },
        timeout_ms: 15_000,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "in-frame confirm button never became visible: {outcome:?}"
    );
    let outcome = submit_primitive(PrimitiveCommand::Click(types::ClickCommand {
        selector: String::new(),
        target: Some(frame_button()),
        boundary: false,
        expected_url: None,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "in-frame click failed: {outcome:?}"
    );

    // The page must still be alive and answer afterwards.
    let outcome =
        submit_primitive(PrimitiveCommand::Inspect(types::InspectCommand::default())).await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "page died after the in-frame click: {outcome:?}"
    );
    server.wait_for_preview_confirmation().await.unwrap();
    let snapshot = server.snapshot().await;
    assert_eq!(snapshot.preview_confirmations, 1);

    runtime.sessions.delete(&session.id).await.unwrap();
}

/// Page-scoped text waits must match plain page text (regression: agents
/// reported {css:body} and {role:main} text waits never matching).
#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn page_scoped_text_wait_matches_body_text() {
    let fixture = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let config = AppConfig {
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
        http: config::HttpConfig {
            allow_loopback: true,
            ..config::HttpConfig::default()
        },
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            shutdown_timeout_ms: 10_000,
        },
        browser: BrowserConfig {
            executable: Some(chrome_executable()),
            profiles_dir: root.path().join("profiles"),
            headless: true,
            max_active: 8,
            upload_roots: vec![root.path().join("uploads")],
            downloads_dir: root.path().join("downloads"),
            artifacts_dir: root.path().join("artifacts"),
            max_artifact_bytes: 8 * 1024 * 1024,
            max_screenshot_dimension: 16_384,
            max_js_result_bytes: 64 * 1024,
            max_js_timeout_ms: 30_000,
        },
        storage: StorageConfig {
            journal_path: root.path().join("commands.jsonl"),
            checkpoints_dir: root.path().join("checkpoints"),
            authority_path: root.path().join("authority.json"),
            scheduler_journal_path: root.path().join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
        context: Default::default(),
        nodes: Default::default(),
    };
    let runtime = RuntimeService::build(&config).await.unwrap();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "text-wait".into(),
            proxy: None,
            execution_policy: Default::default(),
        })
        .await
        .unwrap();
    let page = runtime
        .open_page(OpenPageRequest {
            session_id: session.id.clone(),
        })
        .await
        .unwrap();
    let submit = |command: PrimitiveCommand| {
        runtime.submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(command),
        })
    };
    let outcome = submit(PrimitiveCommand::Navigate(NavigateCommand {
        url: format!("{}/", fixture.base_url()),
        wait_until: WaitUntil::Interactive,
        timeout_ms: 30_000,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    for target in [
        TargetSpec {
            css: Some("body".into()),
            ..TargetSpec::default()
        },
        TargetSpec {
            role: Some("main".into()),
            ..TargetSpec::default()
        },
    ] {
        let outcome = submit(PrimitiveCommand::WaitFor(WaitForCommand {
            condition: WaitCondition::Text {
                target: Box::new(target.clone()),
                matcher: types::TextMatch::Contains("Continue".into()),
            },
            timeout_ms: 5_000,
        }))
        .await;
        assert!(
            matches!(outcome, CommandOutcome::Completed { .. }),
            "page-scoped text wait failed for {target:?}: {outcome:?}"
        );
    }
    runtime.sessions.delete(&session.id).await.unwrap();
}

/// Full agent-path repro: intent submit with a page-scoped text
/// expectedState must observe the post-submit confirmation.
#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn intent_submit_with_text_expected_state_observes_the_confirmation() {
    let server = ScenarioServer::start(ScenarioConfig::seeded("text-expect"))
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let config = AppConfig {
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
        http: config::HttpConfig {
            allow_loopback: true,
            ..config::HttpConfig::default()
        },
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            shutdown_timeout_ms: 10_000,
        },
        browser: BrowserConfig {
            executable: Some(chrome_executable()),
            profiles_dir: root.path().join("profiles"),
            headless: true,
            max_active: 8,
            upload_roots: vec![root.path().join("uploads")],
            downloads_dir: root.path().join("downloads"),
            artifacts_dir: root.path().join("artifacts"),
            max_artifact_bytes: 8 * 1024 * 1024,
            max_screenshot_dimension: 16_384,
            max_js_result_bytes: 64 * 1024,
            max_js_timeout_ms: 30_000,
        },
        storage: StorageConfig {
            journal_path: root.path().join("commands.jsonl"),
            checkpoints_dir: root.path().join("checkpoints"),
            authority_path: root.path().join("authority.json"),
            scheduler_journal_path: root.path().join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
        context: Default::default(),
        nodes: Default::default(),
    };
    let runtime = RuntimeService::build(&config).await.unwrap();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "text-expect".into(),
            proxy: None,
            execution_policy: Default::default(),
        })
        .await
        .unwrap();
    let page = runtime
        .open_page(OpenPageRequest {
            session_id: session.id.clone(),
        })
        .await
        .unwrap();
    let submit = |command: RuntimeCommand| {
        runtime.submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(60),
            command,
        })
    };

    let outcome = submit(RuntimeCommand::Primitive(PrimitiveCommand::Navigate(
        NavigateCommand {
            url: server.application_url("/customers"),
            wait_until: WaitUntil::Interactive,
            timeout_ms: 30_000,
        },
    )))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    // Search and open the customer, mirroring the journey.
    let outcome = submit(RuntimeCommand::Primitive(PrimitiveCommand::TypeText(
        types::TypeTextCommand {
            selector: "input[aria-label='Search customers']".into(),
            target: None,
            value: "Atlas".into(),
            clear_first: true,
            expected_url: None,
        },
    )))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let outcome = submit(RuntimeCommand::Primitive(PrimitiveCommand::Click(
        types::ClickCommand {
            selector: "form[aria-label='Customer search'] button".into(),
            target: None,
            boundary: false,
            expected_url: None,
        },
    )))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let outcome = submit(RuntimeCommand::Primitive(PrimitiveCommand::Click(
        types::ClickCommand {
            selector: "a[href='/customers/cus_atlas']".into(),
            target: None,
            boundary: false,
            expected_url: None,
        },
    )))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    let outcome = submit(RuntimeCommand::Primitive(PrimitiveCommand::WaitFor(
        WaitForCommand {
            condition: WaitCondition::Element {
                target: Box::new(TargetSpec {
                    role: Some("combobox".into()),
                    accessible_name: Some("Customer priority".into()),
                    ..TargetSpec::default()
                }),
                state: types::ElementState::Visible,
            },
            timeout_ms: 5_000,
        },
    )))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "customer detail did not become interactive: {outcome:?}"
    );

    // Set the priority via control action, then submit with the text
    // expectedState the agents used.
    let outcome = submit(RuntimeCommand::Primitive(PrimitiveCommand::ControlAction(
        types::ControlActionCommand {
            target: types::FormControlTarget {
                role: "combobox".into(),
                accessible_name: "Customer priority".into(),
                ordinal: None,
                frame_path: Vec::new(),
                shadow_path: Vec::new(),
            },
            action: types::ControlAction::SelectOne {
                value: "High".into(),
            },
        },
    )))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    // Boundary commands need a matching verified checkpoint: pin the ids.
    let workflow_id = WorkflowId::new();
    let attempt_id = AttemptId::new();
    let command_id = CommandId::new();
    let preflight = runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: workflow_id.clone(),
            attempt_id: attempt_id.clone(),
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(PrimitiveCommand::Inspect(
                types::InspectCommand::default(),
            )),
        })
        .await;
    let CommandOutcome::Completed {
        evidence: observed,
        command_id: inspect_id,
    } = preflight
    else {
        panic!("preflight inspect failed: {preflight:?}")
    };
    let (url, title) = observed
        .iter()
        .find_map(|item| match item {
            types::Evidence::Inspection { url, title, .. } => Some((url.clone(), title.clone())),
            _ => None,
        })
        .unwrap();
    runtime
        .checkpoint(
            types::WorkflowCheckpoint {
                schema_version: types::WorkflowCheckpoint::SCHEMA_VERSION,
                checkpoint_id: types::CheckpointId::new(),
                workflow_id: workflow_id.clone(),
                attempt_id: attempt_id.clone(),
                session_id: session.id.clone(),
                page_id: page.id.clone(),
                restart_url: url.clone(),
                current_url: url.clone(),
                cursor: Some(inspect_id.clone()),
                boundary_command_id: Some(command_id.clone()),
                recovery_class: types::CommandClass::Boundary,
                invariants: vec![
                    types::CheckpointInvariant::Url { value: url },
                    types::CheckpointInvariant::Title { value: title },
                ],
                replayable_inputs: Vec::new(),
                evidence: Vec::new(),
                recovery_history: Vec::new(),
                recovery_receipts: Vec::new(),
                created_at: Utc::now(),
            },
            vec![inspect_id],
        )
        .await
        .unwrap();

    let outcome = runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id,
            workflow_id,
            attempt_id,
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(60),
            command: RuntimeCommand::Intent(IntentCommand::SubmitAndVerify(
                types::SubmitAndVerifyIntent {
                    purpose: "Save the customer priority".into(),
                    hints: IntentHints {
                        role: Some("button".into()),
                        accessible_name: Some("Save priority".into()),
                        ..IntentHints::default()
                    },
                    expected_state: WaitForCommand {
                        condition: WaitCondition::Text {
                            target: Box::new(TargetSpec {
                                css: Some("body".into()),
                                ..TargetSpec::default()
                            }),
                            matcher: types::TextMatch::Contains("Priority saved".into()),
                        },
                        timeout_ms: 20_000,
                    },
                },
            )),
        })
        .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "intent submit with text expectedState failed: {outcome:?}"
    );
    let snapshot = server.snapshot().await;
    assert_eq!(snapshot.atlas_priority, "high");
    runtime.sessions.delete(&session.id).await.unwrap();
}

/// Shared setup: seeded gauntlet server, headless installed-Chromium runtime,
/// documents page with the upload flow completed so the preview iframe is live.
#[allow(dead_code)]
struct DocumentsPageProbe {
    server: ScenarioServer,
    runtime: RuntimeService,
    session_id: types::SessionId,
    page_id: types::PageId,
    _root: tempfile::TempDir,
}

#[allow(dead_code)]
async fn documents_page_with_preview(seed: &str) -> DocumentsPageProbe {
    let server = ScenarioServer::start(ScenarioConfig::seeded(seed))
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/approved-upload.txt");
    let config = AppConfig {
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
        http: config::HttpConfig {
            allow_loopback: true,
            ..config::HttpConfig::default()
        },
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            shutdown_timeout_ms: 10_000,
        },
        browser: BrowserConfig {
            executable: Some(chrome_executable()),
            profiles_dir: root.path().join("profiles"),
            headless: true,
            max_active: 8,
            upload_roots: vec![fixture.parent().unwrap().to_path_buf()],
            downloads_dir: root.path().join("downloads"),
            artifacts_dir: root.path().join("artifacts"),
            max_artifact_bytes: 8 * 1024 * 1024,
            max_screenshot_dimension: 16_384,
            max_js_result_bytes: 64 * 1024,
            max_js_timeout_ms: 30_000,
        },
        storage: StorageConfig {
            journal_path: root.path().join("commands.jsonl"),
            checkpoints_dir: root.path().join("checkpoints"),
            authority_path: root.path().join("authority.json"),
            scheduler_journal_path: root.path().join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
        context: Default::default(),
        nodes: Default::default(),
    };
    let runtime = RuntimeService::build(&config).await.unwrap();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: seed.into(),
            proxy: None,
            execution_policy: Default::default(),
        })
        .await
        .unwrap();
    let page = runtime
        .open_page(OpenPageRequest {
            session_id: session.id.clone(),
        })
        .await
        .unwrap();
    let submit = |command: PrimitiveCommand| {
        runtime.submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(command),
        })
    };
    let outcome = submit(PrimitiveCommand::Navigate(NavigateCommand {
        url: server.application_url("/customers/cus_atlas/documents"),
        wait_until: WaitUntil::Interactive,
        timeout_ms: 30_000,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let form_snapshot = runtime
        .form_snapshot(&session.id, &page.id, None)
        .await
        .unwrap();
    let file_target = form_snapshot
        .forms
        .iter()
        .flat_map(|form| form.controls.iter())
        .chain(form_snapshot.unowned_controls.iter())
        .find(|control| control.control_kind == FormControlKind::File)
        .and_then(|control| control.target.as_ref())
        .map(target_spec)
        .expect("file input target from form snapshot");
    let outcome = submit(PrimitiveCommand::UploadFiles(UploadFilesCommand {
        selector: String::new(),
        target: Some(file_target),
        paths: vec![fixture.to_string_lossy().into_owned()],
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let outcome = submit(PrimitiveCommand::Click(types::ClickCommand {
        selector: "form[aria-label='Upload customer document'] button".into(),
        target: None,
        boundary: false,
        expected_url: None,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let outcome = submit(PrimitiveCommand::WaitFor(WaitForCommand {
        condition: WaitCondition::Element {
            target: Box::new(TargetSpec {
                css: Some("iframe#document-preview".into()),
                ..TargetSpec::default()
            }),
            state: ElementState::Attached,
        },
        timeout_ms: 15_000,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    DocumentsPageProbe {
        server,
        runtime,
        session_id: session.id,
        page_id: page.id,
        _root: root,
    }
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn control_action_reports_revealed_conditional_controls() {
    let server = ScenarioServer::start(ScenarioConfig::seeded("revealed-controls"))
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/approved-upload.txt");
    let config = AppConfig {
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
        http: config::HttpConfig {
            allow_loopback: true,
            ..config::HttpConfig::default()
        },
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            shutdown_timeout_ms: 10_000,
        },
        browser: BrowserConfig {
            executable: Some(chrome_executable()),
            profiles_dir: root.path().join("profiles"),
            headless: true,
            max_active: 8,
            upload_roots: vec![fixture.parent().unwrap().to_path_buf()],
            downloads_dir: root.path().join("downloads"),
            artifacts_dir: root.path().join("artifacts"),
            max_artifact_bytes: 8 * 1024 * 1024,
            max_screenshot_dimension: 16_384,
            max_js_result_bytes: 64 * 1024,
            max_js_timeout_ms: 30_000,
        },
        storage: StorageConfig {
            journal_path: root.path().join("commands.jsonl"),
            checkpoints_dir: root.path().join("checkpoints"),
            authority_path: root.path().join("authority.json"),
            scheduler_journal_path: root.path().join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
        context: Default::default(),
        nodes: Default::default(),
    };
    let runtime = RuntimeService::build(&config).await.unwrap();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "revealed-controls".into(),
            proxy: None,
            execution_policy: Default::default(),
        })
        .await
        .unwrap();
    let page = runtime
        .open_page(OpenPageRequest {
            session_id: session.id.clone(),
        })
        .await
        .unwrap();
    let submit = |command: PrimitiveCommand| {
        runtime.submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(command),
        })
    };
    let outcome = submit(PrimitiveCommand::Navigate(NavigateCommand {
        url: server.application_url("/onboarding"),
        wait_until: WaitUntil::Interactive,
        timeout_ms: 30_000,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    // Selecting the growth plan reveals the Billing cycle select, which does
    // not exist in the snapshot taken beforehand.
    let outcome = submit(PrimitiveCommand::ControlAction(
        types::ControlActionCommand {
            target: types::FormControlTarget {
                role: "combobox".into(),
                accessible_name: "Plan".into(),
                ordinal: None,
                frame_path: Vec::new(),
                shadow_path: Vec::new(),
            },
            action: types::ControlAction::SelectOne {
                value: "growth".into(),
            },
        },
    ))
    .await;
    let CommandOutcome::Completed { evidence, .. } = outcome else {
        panic!("plan select failed: {outcome:?}");
    };
    let revealed = evidence.iter().find_map(|item| match item {
        types::Evidence::ControlAction { action } => Some(action.revealed_controls.clone()),
        _ => None,
    });
    let revealed = revealed.expect("control action evidence");
    let billing = revealed
        .iter()
        .find(|control| control.accessible_name.as_deref() == Some("Billing cycle"));
    let billing = billing.unwrap_or_else(|| {
        panic!("Billing cycle select missing from revealed controls: {revealed:?}")
    });
    let target = billing.target.as_ref().expect("revealed control target");
    assert_eq!(target.role, "combobox");
    runtime.sessions.delete(&session.id).await.unwrap();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn a11y_snapshot_exposes_link_urls() {
    let server = ScenarioServer::start(ScenarioConfig::seeded("a11y-link-urls"))
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/approved-upload.txt");
    let config = AppConfig {
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
        http: config::HttpConfig {
            allow_loopback: true,
            ..config::HttpConfig::default()
        },
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            shutdown_timeout_ms: 10_000,
        },
        browser: BrowserConfig {
            executable: Some(chrome_executable()),
            profiles_dir: root.path().join("profiles"),
            headless: true,
            max_active: 8,
            upload_roots: vec![fixture.parent().unwrap().to_path_buf()],
            downloads_dir: root.path().join("downloads"),
            artifacts_dir: root.path().join("artifacts"),
            max_artifact_bytes: 8 * 1024 * 1024,
            max_screenshot_dimension: 16_384,
            max_js_result_bytes: 64 * 1024,
            max_js_timeout_ms: 30_000,
        },
        storage: StorageConfig {
            journal_path: root.path().join("commands.jsonl"),
            checkpoints_dir: root.path().join("checkpoints"),
            authority_path: root.path().join("authority.json"),
            scheduler_journal_path: root.path().join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
        context: Default::default(),
        nodes: Default::default(),
    };
    let runtime = RuntimeService::build(&config).await.unwrap();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "a11y-link-urls".into(),
            proxy: None,
            execution_policy: Default::default(),
        })
        .await
        .unwrap();
    let page = runtime
        .open_page(OpenPageRequest {
            session_id: session.id.clone(),
        })
        .await
        .unwrap();
    let submit = |command: PrimitiveCommand| {
        runtime.submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(45),
            command: RuntimeCommand::Primitive(command),
        })
    };
    let outcome = submit(PrimitiveCommand::Navigate(NavigateCommand {
        url: server.application_url("/reports"),
        wait_until: WaitUntil::Interactive,
        timeout_ms: 30_000,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let outcome = submit(PrimitiveCommand::Click(types::ClickCommand {
        selector: "form[aria-label='Generate report'] button".into(),
        target: None,
        boundary: false,
        expected_url: None,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let outcome = submit(PrimitiveCommand::WaitFor(WaitForCommand {
        condition: WaitCondition::Text {
            target: Box::new(TargetSpec {
                css: Some("body".into()),
                ..TargetSpec::default()
            }),
            matcher: types::TextMatch::Contains("Report ready".into()),
        },
        timeout_ms: 30_000,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    let outcome = submit(PrimitiveCommand::AccessibilitySnapshot(
        types::AccessibilitySnapshotCommand {
            max_nodes: None,
            target: None,
        },
    ))
    .await;
    let CommandOutcome::Completed { evidence, .. } = outcome else {
        panic!("a11y snapshot failed: {outcome:?}");
    };
    let nodes = evidence
        .iter()
        .find_map(|item| match item {
            types::Evidence::AccessibilitySnapshot { nodes, .. } => Some(nodes),
            _ => None,
        })
        .expect("snapshot evidence");
    fn flatten<'a>(
        nodes: &'a [types::AccessibilityNode],
        out: &mut Vec<&'a types::AccessibilityNode>,
    ) {
        for node in nodes {
            out.push(node);
            flatten(&node.children, out);
        }
    }
    let mut flat = Vec::new();
    flatten(nodes, &mut flat);
    let link = flat
        .iter()
        .find(|node| {
            node.role.as_deref() == Some("link")
                && node
                    .name
                    .as_deref()
                    .is_some_and(|name| name.contains("atlas-operations.csv"))
        })
        .expect("download link missing from a11y snapshot");
    let url = link.url.as_deref().expect("download link carries no url");
    assert!(
        url.contains("/api/reports/") && url.ends_with("/download"),
        "unexpected link url: {url}"
    );
    runtime.sessions.delete(&session.id).await.unwrap();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn a11y_snapshot_scopes_to_a_target_subtree() {
    let probe = documents_page_with_preview("a11y-scoped").await;
    let snapshot = |target: TargetSpec| {
        probe.runtime.submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: probe.session_id.clone(),
            page_id: Some(probe.page_id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(PrimitiveCommand::AccessibilitySnapshot(
                types::AccessibilitySnapshotCommand {
                    max_nodes: None,
                    target: Some(target),
                },
            )),
        })
    };
    fn flatten_owned(nodes: &[types::AccessibilityNode], out: &mut Vec<types::AccessibilityNode>) {
        for node in nodes {
            out.push(node.clone());
            flatten_owned(&node.children, out);
        }
    }
    let nodes_of = |outcome: CommandOutcome| {
        let CommandOutcome::Completed { evidence, .. } = outcome else {
            panic!("scoped snapshot failed: {outcome:?}");
        };
        let nodes = evidence
            .iter()
            .find_map(|item| match item {
                types::Evidence::AccessibilitySnapshot { nodes, .. } => Some(nodes.clone()),
                _ => None,
            })
            .expect("snapshot evidence");
        let mut flat = Vec::new();
        flatten_owned(&nodes, &mut flat);
        flat
    };

    // Main-frame scope: the upload form subtree carries its controls and none
    // of the page chrome around it.
    let flat = nodes_of(
        snapshot(TargetSpec {
            role: Some("form".into()),
            accessible_name: Some("Upload customer document".into()),
            ..TargetSpec::default()
        })
        .await,
    );
    assert!(
        flat.iter()
            .any(|node| node.name.as_deref() == Some("Upload document")),
        "scoped form snapshot lost its submit button: {flat:?}"
    );
    assert!(
        !flat
            .iter()
            .any(|node| node.name.as_deref() == Some("CUSTOMER RECORDS")),
        "scoped snapshot leaked page chrome: {flat:?}"
    );

    // In-frame scope: the preview document's main element carries the confirm
    // button without the outer page.
    let flat = nodes_of(
        snapshot(TargetSpec {
            role: Some("main".into()),
            frame_path: vec![Box::new(TargetSpec {
                css: Some("#document-preview".into()),
                ..TargetSpec::default()
            })],
            ..TargetSpec::default()
        })
        .await,
    );
    assert!(
        flat.iter()
            .any(|node| node.name.as_deref() == Some("Confirm document preview")),
        "in-frame scoped snapshot lost the confirm button: {flat:?}"
    );
    assert!(
        !flat
            .iter()
            .any(|node| node.name.as_deref() == Some("Upload document")),
        "in-frame scoped snapshot leaked the outer page: {flat:?}"
    );
    probe
        .runtime
        .sessions
        .delete(&probe.session_id)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn a11y_snapshot_descends_into_iframes() {
    let probe = documents_page_with_preview("a11y-frames").await;
    let evidence = probe
        .runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: probe.session_id.clone(),
            page_id: Some(probe.page_id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(PrimitiveCommand::AccessibilitySnapshot(
                types::AccessibilitySnapshotCommand {
                    max_nodes: None,
                    target: None,
                },
            )),
        })
        .await;
    let CommandOutcome::Completed { evidence, .. } = evidence else {
        panic!("a11y snapshot failed: {evidence:?}");
    };
    let nodes = evidence
        .iter()
        .find_map(|item| match item {
            types::Evidence::AccessibilitySnapshot { nodes, .. } => Some(nodes),
            _ => None,
        })
        .expect("snapshot evidence");
    fn flatten<'a>(
        nodes: &'a [types::AccessibilityNode],
        out: &mut Vec<&'a types::AccessibilityNode>,
    ) {
        for node in nodes {
            out.push(node);
            flatten(&node.children, out);
        }
    }
    let mut flat = Vec::new();
    flatten(nodes, &mut flat);
    for node in &flat {
        if node.role.is_some() || node.name.is_some() {
            println!(
                "role={:?} name={:?} target={:?}",
                node.role, node.name, node.target
            );
        }
    }
    let confirm = flat.iter().find(|node| {
        node.name.as_deref() == Some("Confirm document preview")
            && node.role.as_deref() == Some("button")
    });
    let confirm = confirm.expect("in-frame confirm button missing from a11y snapshot");
    let target = confirm
        .target
        .as_ref()
        .expect("confirm button has no target");
    assert!(
        !target.frame_path.is_empty(),
        "in-frame target must carry its frame hop: {target:?}"
    );

    // The whole point: pass the stamped target verbatim to control_action
    // and have the activation land inside the frame.
    let outcome = probe
        .runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: probe.session_id.clone(),
            page_id: Some(probe.page_id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(PrimitiveCommand::ControlAction(
                types::ControlActionCommand {
                    target: types::FormControlTarget {
                        role: target.role.clone(),
                        accessible_name: target.accessible_name.clone(),
                        ordinal: target.ordinal,
                        frame_path: target.frame_path.clone(),
                        shadow_path: Vec::new(),
                    },
                    action: types::ControlAction::Activate,
                },
            )),
        })
        .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "control_action with the snapshot target failed: {outcome:?}"
    );
    probe
        .server
        .wait_for_preview_confirmation()
        .await
        .expect("in-frame activation did not land");
    probe
        .runtime
        .sessions
        .delete(&probe.session_id)
        .await
        .unwrap();
}
