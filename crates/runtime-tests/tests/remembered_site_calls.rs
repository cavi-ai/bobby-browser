//! What remembering a site saves an agent, on managed Chromium.
//!
//! The agent fills the gauntlet onboarding identity form with the two MCP
//! calls a real driver made on this journey: `workflow_observe` with a goal,
//! then `intent_complete_form`. It runs once against a cold context store and
//! once more after a runtime restart over the same store. The browser profile
//! is disposable both times; only the context store carries over.
//!
//! Run with `--nocapture` to print the per-call table the docs reproduce.

#[allow(dead_code)]
#[path = "modern_gauntlet/mod.rs"]
mod modern_gauntlet;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{Duration, Utc};
use mcp_gateway::Server;
use modern_gauntlet::scenario::{ScenarioConfig, ScenarioServer};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use types::{
    AttemptId, Capability, CommandEnvelope, CommandId, CommandOutcome, CreateSessionRequest,
    NavigateCommand, OpenPageRequest, PrimitiveCommand, RuntimeCommand, WaitUntil, WorkflowId,
};
use worker_pool::ChromiumWorkerFactory;

const CAPABILITIES: [Capability; 7] = [
    Capability::SessionRead,
    Capability::SessionWrite,
    Capability::PageRead,
    Capability::PageWrite,
    Capability::BrowserMutate,
    Capability::IntentExecute,
    Capability::ContextRead,
];

fn chrome_executable() -> PathBuf {
    std::env::var("BOBBY_CHROME_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        })
}

fn config(root: &Path) -> config::AppConfig {
    config::AppConfig {
        http: config::HttpConfig {
            allow_loopback: true,
            ..config::HttpConfig::default()
        },
        server: config::ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            shutdown_timeout_ms: 10_000,
        },
        browser: config::BrowserConfig {
            executable: Some(chrome_executable()),
            profiles_dir: root.join("profiles"),
            headless: true,
            max_active: 1,
            upload_roots: vec![],
            downloads_dir: root.join("downloads"),
            artifacts_dir: root.join("artifacts"),
            max_artifact_bytes: 8 * 1024 * 1024,
            max_screenshot_dimension: 16_384,
            max_js_result_bytes: 64 * 1024,
            max_js_timeout_ms: 30_000,
        },
        storage: config::StorageConfig {
            journal_path: root.join("commands.jsonl"),
            checkpoints_dir: root.join("checkpoints"),
            authority_path: root.join("authority.json"),
            scheduler_journal_path: root.join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
        context: config::ContextConfig {
            dir: Some(root.join("context")),
            ..config::ContextConfig::default()
        },
        nodes: Default::default(),
    }
}

/// One MCP call as the agent saw it.
#[derive(Debug)]
struct Call {
    tool: &'static str,
    bytes: usize,
    millis: u128,
    status: Value,
    source: Value,
}

/// The two agent calls on one runtime, then the session closes and every
/// handle on the runtime drops, so the next run starts a fresh process state
/// over the same context store.
async fn agent_run(root: &Path, url: &str) -> Vec<Call> {
    let config = config(root);
    let profile_id = config::EnginePreferenceConfig::ManagedChromium
        .durable_profile_id()
        .expect("managed Chromium carries a memory identity");
    let runtime = RuntimeService::build_with_context_promotion(
        &config,
        Arc::new(ChromiumWorkerFactory::new(config.browser.clone())),
        profile_id,
    )
    .await
    .unwrap();
    let authority = interface_core::AuthorityStore::in_memory();
    let token = authority
        .issue(
            types::PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            CAPABILITIES,
            Utc::now() + Duration::minutes(10),
        )
        .await
        .unwrap()
        .expose_once();
    let handle = authority.verify(&token).await.unwrap();
    let server = Server::new(Arc::new(AuthenticatedRuntime::new(runtime.clone(), handle)));
    server
        .handle_message(json!({"jsonrpc":"2.0","id":0,"method":"initialize",
            "params":{"protocolVersion":"2025-11-25","capabilities":{},
                      "clientInfo":{"name":"remembered-site","version":"1"}}}))
        .await
        .unwrap();
    server
        .handle_message(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .await;

    // Setup, identical in both runs and not counted: open a page on the
    // onboarding form behind the gauntlet's sign-in and MFA.
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "remembered-site".into(),
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
    let navigated = runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
                url: url.into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            })),
        })
        .await;
    assert!(
        matches!(navigated, CommandOutcome::Completed { .. }),
        "{navigated:?}"
    );
    modern_gauntlet::unlock::unlock_northstar_session(&runtime, &session.id, &page.id)
        .await
        .unwrap();

    let ids = json!({"sessionId":session.id,"pageId":page.id});
    let mut calls = Vec::new();
    for (id, tool, arguments) in [
        (1, "workflow_observe", json!({"goal":"Full name"})),
        (
            2,
            "intent_complete_form",
            json!({
                "purpose":"Customer identity",
                "fields":[
                    {"name":"fullName","purpose":"Full name",
                     "hints":{"role":"textbox","accessibleName":"Full name"},
                     "value":{"kind":"setText","value":"Maya Chen","clearFirst":true}},
                    {"name":"workEmail","purpose":"Work email",
                     "hints":{"role":"textbox","accessibleName":"Work email"},
                     "value":{"kind":"setText","value":"maya@atlas.example","clearFirst":true}}
                ]
            }),
        ),
    ] {
        let mut arguments = arguments;
        arguments["sessionId"] = ids["sessionId"].clone();
        arguments["pageId"] = ids["pageId"].clone();
        let started = std::time::Instant::now();
        let response = server
            .handle_message(json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                                   "params":{"name":tool,"arguments":arguments}}))
            .await
            .expect("tools/call answers");
        assert!(response.get("error").is_none(), "{tool}: {response}");
        let result = &response["result"];
        calls.push(Call {
            tool,
            bytes: serde_json::to_string(result).unwrap().len(),
            millis: started.elapsed().as_millis(),
            status: result["structuredContent"]["status"].clone(),
            source: result["structuredContent"]["source"].clone(),
        });
    }
    // The agent never calls session_close, as the driver on this journey
    // did not. Tearing the browser down directly skips the interface's close
    // path, so whatever survives the restart was written when it was verified.
    runtime.sessions.delete(&session.id).await.unwrap();
    calls
}

#[tokio::test]
#[ignore = "requires installed Chromium"]
async fn a_remembered_site_skips_the_snapshot_after_a_restart() {
    let server = ScenarioServer::start(ScenarioConfig::seeded("remembered-site"))
        .await
        .unwrap();
    let url = server.application_url("/onboarding");
    let root = tempfile::tempdir().unwrap();

    let cold = agent_run(root.path(), &url).await;
    let remembered = agent_run(root.path(), &url).await;

    println!("| Call | Cold bytes | Cold ms | Cold source | Remembered bytes | Remembered ms | Remembered source |");
    for (cold, remembered) in cold.iter().zip(&remembered) {
        println!(
            "| {} | {} | {} | {} | {} | {} | {} |",
            cold.tool,
            cold.bytes,
            cold.millis,
            cold.source,
            remembered.bytes,
            remembered.millis,
            remembered.source
        );
    }

    for call in cold.iter().chain(&remembered) {
        assert_eq!(call.status, "completed", "{call:?}");
    }
    assert_eq!(cold.len(), remembered.len());
    assert_eq!(cold[0].source, "live", "{cold:?}");
    assert_eq!(
        remembered[0].source, "retained",
        "the restarted runtime did not answer from memory: {remembered:?}"
    );
    assert!(
        remembered[0].bytes < cold[0].bytes,
        "a remembered observation read more than a live one: {remembered:?} vs {cold:?}"
    );
}
