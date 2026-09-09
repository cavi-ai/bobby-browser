//! Live installed-Chromium proof: a popup opened by `window.open` becomes
//! visible and addressable through `ListPages` without the dedicated
//! click-and-wait command.

use std::path::PathBuf;

use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, ServerConfig, StorageConfig};
use gauntlet_server::{ScenarioConfig, ScenarioServer};
use sdk_core::RuntimeService;
use types::{
    AttemptId, ClickCommand, CommandEnvelope, CommandId, CommandOutcome, CreateSessionRequest,
    EvaluateJavaScriptCommand, Evidence, ListPagesCommand, NavigateCommand, OpenPageRequest,
    PrimitiveCommand, RuntimeCommand, WaitUntil, WorkflowId,
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
async fn popup_opens_as_a_listed_page() {
    let server = ScenarioServer::start(ScenarioConfig::seeded("popup-discovery"))
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
            profile: "popup-discovery".into(),
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
        url: server.application_url("/integrations"),
        wait_until: WaitUntil::Interactive,
        timeout_ms: 30_000,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    let outcome = submit(PrimitiveCommand::Click(ClickCommand {
        selector: "button[aria-label='Connect Ledger Cloud']".into(),
        target: None,
        boundary: false,
        expected_url: None,
        modifiers: Vec::new(),
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    // The popup must appear in the listing without any dedicated wait.
    let outcome = submit(PrimitiveCommand::ListPages(ListPagesCommand)).await;
    let CommandOutcome::Completed { evidence, .. } = outcome else {
        panic!("list_pages did not complete: {outcome:?}")
    };
    let urls: Vec<String> = evidence
        .iter()
        .find_map(|item| match item {
            Evidence::Pages { pages } => Some(pages.iter().map(|page| page.url.clone()).collect()),
            _ => None,
        })
        .unwrap_or_default();
    assert_eq!(urls.len(), 2, "expected opener plus popup, got {urls:?}");
    assert!(
        urls.iter().any(|url| url.contains("ledger")),
        "popup URL not listed: {urls:?}"
    );

    runtime.sessions.delete(&session.id).await.unwrap();
}

/// P4 repro: the popup closes itself (`window.close()`, the way the
/// authorization page does), and the opener must still be listed afterward.
/// Reported symptom: `list_pages` came back `pages: []` -- opener included --
/// even though the opener answered the very next call.
#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn popup_closed_from_inside_still_lists_the_opener() {
    let server = ScenarioServer::start(ScenarioConfig::seeded("popup-discovery-close"))
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
            profile: "popup-discovery-close".into(),
            proxy: None,
            execution_policy: Default::default(),
            zigzagzig: false,
        })
        .await
        .unwrap();
    let opener = runtime
        .open_page(OpenPageRequest {
            session_id: session.id.clone(),
        })
        .await
        .unwrap();

    let submit = |page_id: types::PageId, command: PrimitiveCommand| {
        runtime.submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session.id.clone(),
            page_id: Some(page_id),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(command),
        })
    };

    let opener_url = server.application_url("/integrations");
    let outcome = submit(
        opener.id.clone(),
        PrimitiveCommand::Navigate(NavigateCommand {
            url: opener_url.clone(),
            wait_until: WaitUntil::Interactive,
            timeout_ms: 30_000,
        }),
    )
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    let outcome = submit(
        opener.id.clone(),
        PrimitiveCommand::Click(ClickCommand {
            selector: "button[aria-label='Connect Ledger Cloud']".into(),
            target: None,
            boundary: false,
            expected_url: None,
            modifiers: Vec::new(),
        }),
    )
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    let outcome = submit(
        opener.id.clone(),
        PrimitiveCommand::ListPages(ListPagesCommand),
    )
    .await;
    let CommandOutcome::Completed { evidence, .. } = outcome else {
        panic!("list_pages did not complete: {outcome:?}")
    };
    let popup_id = evidence
        .iter()
        .find_map(|item| match item {
            Evidence::Pages { pages } => pages
                .iter()
                .find(|page| page.url.contains("ledger"))
                .map(|page| page.page_id.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("popup page not listed: {evidence:?}"));

    // Close the popup from inside it -- the same path the authorization page
    // uses once the user finishes. The close tears the target down out from
    // under the eval's own reply, so either a completed eval or a
    // page-not-open failure on *this* call is acceptable; what matters is
    // what `list_pages` says next.
    let _ = submit(
        popup_id,
        PrimitiveCommand::EvaluateJavaScript(EvaluateJavaScriptCommand {
            expression: "window.close()".into(),
            timeout_ms: 5_000,
            await_promise: false,
        }),
    )
    .await;

    let outcome = submit(
        opener.id.clone(),
        PrimitiveCommand::ListPages(ListPagesCommand),
    )
    .await;
    let CommandOutcome::Completed { evidence, .. } = outcome else {
        panic!("list_pages did not complete after popup close: {outcome:?}")
    };
    let urls: Vec<String> = evidence
        .iter()
        .find_map(|item| match item {
            Evidence::Pages { pages } => Some(pages.iter().map(|page| page.url.clone()).collect()),
            _ => None,
        })
        .unwrap_or_default();
    assert!(
        urls.iter().any(|url| url == &opener_url),
        "opener missing from list_pages after the popup closed itself: {urls:?}"
    );

    runtime.sessions.delete(&session.id).await.unwrap();
}
