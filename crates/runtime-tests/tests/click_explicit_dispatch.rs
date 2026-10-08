//! Live installed-Chromium proof: a plain click (no modifiers, humanization
//! off) dispatches through the same explicit move/press/release sequence as
//! a modifier click. One click on `/reports`' "Generate report" button
//! produces exactly one report generation.

use std::path::PathBuf;

use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, ServerConfig, StorageConfig};
use sdk_core::RuntimeService;
use test_site::{FixtureSite, Route};
use types::{
    AttemptId, ClickCommand, CommandEnvelope, CommandId, CommandOutcome, CreateSessionRequest,
    NavigateCommand, OpenPageRequest, PrimitiveCommand, RuntimeCommand, TargetSpec, WaitUntil,
    WorkflowId,
};

const REPORTS: &str = r#"<!doctype html><title>Reports</title>
<main><h1>Reports</h1>
<button id="generate">Generate report</button>
<script>document.getElementById("generate").addEventListener("click", () => {
  fetch("/api/report", { method: "POST" });
});</script></main>"#;

fn chrome_executable() -> PathBuf {
    std::env::var("BOBBY_CHROME_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        })
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn plain_click_through_the_explicit_sequence_generates_one_report() {
    let server = FixtureSite::spawn(vec![
        ("/reports", Route::Html(REPORTS.into())),
        (
            "/api/report",
            Route::Raw {
                content_type: "application/json",
                body: "{}".into(),
            },
        ),
    ])
    .await;
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
            profile: "plain-click-explicit-dispatch".into(),
            proxy: None,
            execution_policy: Default::default(),
            zigzagzig: false,
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
        url: server.url("/reports"),
        wait_until: WaitUntil::Interactive,
        timeout_ms: 30_000,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    // No modifiers and humanization off (the default): the plain-click shape.
    let outcome = submit(PrimitiveCommand::Click(ClickCommand {
        selector: String::new(),
        target: Some(TargetSpec {
            role: Some("button".into()),
            accessible_name: Some("Generate report".into()),
            ..TargetSpec::default()
        }),
        boundary: false,
        expected_url: None,
        modifiers: Vec::new(),
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    for _ in 0..100 {
        if server.hits("/api/report") > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    // Let a duplicate dispatch, if any, land before counting.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(
        server.hits("/api/report"),
        1,
        "expected exactly one report generation from one click"
    );

    runtime.sessions.delete(&session.id).await.unwrap();
}
