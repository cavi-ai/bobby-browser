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
    IntentCommand, IntentHints, LocateIntent, NavigateCommand, OpenPageRequest, PrimitiveCommand,
    RuntimeCommand, TargetSpec, UploadFilesCommand, WaitCondition, WaitForCommand, WaitUntil,
    WorkflowId,
};

fn chrome_executable() -> PathBuf {
    std::env::var("BOBBY_CHROME_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        })
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

    let outcome = submit_primitive(PrimitiveCommand::UploadFiles(UploadFilesCommand {
        selector: "input[aria-label='Customer document']".into(),
        target: None,
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

    // The confirm button lives inside the preview iframe. No framePath: the
    // gather must descend and resolve it anyway.
    let outcome = submit_intent(IntentCommand::Locate(LocateIntent {
        purpose: "Confirm document preview".into(),
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

    runtime.sessions.delete(&session.id).await.unwrap();
}
