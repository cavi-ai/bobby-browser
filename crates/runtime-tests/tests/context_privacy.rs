//! Spec C privacy canary (release gate): a fill+submit workflow with canary
//! values runs through the live harness with context promotion attached;
//! afterwards every byte under the context store is scanned and the canary
//! must be absent. The store must also be non-empty — a scan over an empty
//! store proves nothing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, ServerConfig, StorageConfig};
use sdk_core::RuntimeService;
use types::{
    AttemptId, CommandEnvelope, CommandId, CommandOutcome, ControlAction, CreateSessionRequest,
    Evidence, FillIntent, IntentCommand, IntentHints, NavigateCommand, OpenPageRequest, PageId,
    PrimitiveCommand, RuntimeCommand, SessionId, WaitUntil, WorkflowId,
};
use worker_pool::ChromiumWorkerFactory;

const CANARY: &str = "canary-7f3c9e1b-typed-value-never-persisted";

fn chrome_executable() -> PathBuf {
    std::env::var("BOBBY_CHROME_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        })
}

async fn submit(
    runtime: &RuntimeService,
    session_id: &SessionId,
    page_id: &PageId,
    command: RuntimeCommand,
) -> Vec<Evidence> {
    match runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session_id.clone(),
            page_id: Some(page_id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command,
        })
        .await
    {
        CommandOutcome::Completed { evidence, .. } => evidence,
        outcome => panic!("command did not complete: {outcome:?}"),
    }
}

fn scan_for_canary(dir: &Path) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                hits.extend(scan_for_canary(&path));
            } else if let Ok(bytes) = std::fs::read(&path) {
                if bytes
                    .windows(CANARY.len())
                    .any(|window| window == CANARY.as_bytes())
                {
                    hits.push(path);
                }
            }
        }
    }
    hits
}

/// Every `.json` under `root`, at any depth.
fn json_files_under(root: &std::path::Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
                found.push(path);
            }
        }
    }
    found
}

#[tokio::test]
#[ignore = "requires installed Chromium"]
async fn typed_values_never_reach_the_context_store() {
    let fixture = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let context_dir = root.path().join("context");
    let config = AppConfig {
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
            max_active: 1,
            upload_roots: vec![],
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
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
        context: config::ContextConfig {
            dir: Some(context_dir.clone()),
            ..config::ContextConfig::default()
        },
        nodes: Default::default(),
    };
    let factory = Arc::new(ChromiumWorkerFactory::new(config.browser.clone()));
    let runtime = RuntimeService::build_with_context_promotion(&config, factory, "canary-profile")
        .await
        .unwrap();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "canary-profile".into(),
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
    submit(
        &runtime,
        &session.id,
        &page.id,
        RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
            url: fixture.base_url(),
            wait_until: WaitUntil::Interactive,
            timeout_ms: 10_000,
        })),
    )
    .await;
    submit(
        &runtime,
        &session.id,
        &page.id,
        RuntimeCommand::Intent(IntentCommand::Fill(FillIntent {
            purpose: "Name".into(),
            hints: IntentHints {
                role: Some("textbox".into()),
                ..IntentHints::default()
            },
            value: ControlAction::SetText {
                value: CANARY.into(),
                clear_first: true,
            },
        })),
    )
    .await;

    // Session close is the flush point; close through the authenticated
    // interface so the flush path under test is the production one.
    let authority = interface_core::AuthorityStore::in_memory();
    let handle = authority
        .issue(
            types::PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [
                types::Capability::SessionWrite,
                types::Capability::SessionRead,
            ],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let handle = authority.verify(&handle).await.unwrap();
    let principal = handle.principal_id().clone();
    let authed = sdk_core::AuthenticatedRuntime::new(runtime.clone(), handle);
    interface_core::RuntimeInterface::delete_session(
        &authed,
        types::RequestContext::new_for_test(
            principal,
            [
                types::Capability::SessionWrite,
                types::Capability::SessionRead,
            ],
            Utc::now() + Duration::minutes(1),
        ),
        session.id.clone(),
    )
    .await
    .unwrap();

    // Walk the whole store, rather than naming a profile directory: the store
    // hex-encodes the profile component, so `canary-profile` is never a
    // literal path. Walking is also what this canary is for -- every byte
    // under the root gets scanned, including any file a future layout adds.
    let site_files: Vec<PathBuf> = json_files_under(&context_dir);
    assert!(
        !site_files.is_empty(),
        "context store was never written under {}; the canary scan would be vacuous",
        context_dir.display()
    );
    let persisted: String = site_files
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect();
    assert!(
        persisted.contains("\"Name\"") || persisted.contains("Name"),
        "remembered control structure missing from the store: {persisted}"
    );
    let hits = scan_for_canary(&context_dir);
    assert!(
        hits.is_empty(),
        "typed canary value leaked into the context store: {hits:?}"
    );
}
