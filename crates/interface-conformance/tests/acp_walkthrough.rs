//! The ACP walkthrough on the docs page, on real Chromium: an editor opens a
//! page and fills a field, asks the retained context, saves a checkpoint,
//! reads recovery status, recovers, and closes. MCP asked the same context
//! and recovery questions against the same runtime and principal must answer
//! identically.
//!
//! The ACP side runs the gateway's `AcpServer` over a byte pipe with the same
//! newline-delimited JSON-RPC framing `acp-gateway` speaks on stdio. Run with
//! `--nocapture` to print the transcript the docs page reproduces.

use std::sync::Arc;

use acp_gateway::AcpServer;
use chrono::{Duration, Utc};
use interface_conformance::live::ChromeRuntimeHarness;
use mcp_gateway::Server;
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, ReadHalf, WriteHalf};
use types::{Capability, PrincipalId};
use worker_pool::ChromiumWorkerFactory;

const CAPABILITIES: [Capability; 9] = [
    Capability::SessionRead,
    Capability::SessionWrite,
    Capability::PageRead,
    Capability::PageWrite,
    Capability::BrowserMutate,
    Capability::IntentExecute,
    Capability::ContextRead,
    Capability::RecoveryRead,
    Capability::RecoveryWrite,
];

struct Editor {
    writer: WriteHalf<tokio::io::DuplexStream>,
    lines: Lines<BufReader<ReadHalf<tokio::io::DuplexStream>>>,
    next_id: u64,
}

impl Editor {
    async fn send(&mut self, method: &str, params: Value) -> (Value, Vec<Value>) {
        self.next_id += 1;
        let id = self.next_id;
        let frame = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        self.writer
            .write_all(format!("{frame}\n").as_bytes())
            .await
            .unwrap();
        let mut updates = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let line = tokio::time::timeout_at(deadline, self.lines.next_line())
                .await
                .unwrap_or_else(|_| panic!("{method}: no answer within 60s"))
                .unwrap()
                .expect("gateway closed the pipe");
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame["method"] == "session/update" {
                updates.push(frame["params"].clone());
                continue;
            }
            if frame["id"] == id {
                assert!(frame.get("error").is_none(), "{method} failed: {frame}");
                return (frame["result"].clone(), updates);
            }
        }
    }

    /// One structured prompt; returns the JSON payload the agent streamed back.
    async fn prompt(&mut self, session: &str, request: Value) -> Value {
        println!("editor -> session/prompt {request}");
        let (result, updates) = self
            .send(
                "session/prompt",
                json!({"sessionId":session,"prompt":[{"type":"text","text":request.to_string()}]}),
            )
            .await;
        let payload = updates
            .iter()
            .filter_map(|update| update.pointer("/update/content/text"))
            .filter_map(Value::as_str)
            .find_map(|text| serde_json::from_str::<Value>(text).ok())
            .expect("a structured session/update payload");
        println!(
            "bobby  <- session/update {} ; stopReason {}",
            summarize(&payload),
            result["stopReason"]
        );
        assert_eq!(result["stopReason"], "end_turn", "{payload}");
        payload
    }
}

/// The transcript line for one payload: the operation and its load-bearing fields.
fn summarize(payload: &Value) -> Value {
    let result = &payload["result"];
    match payload["operation"].as_str() {
        Some("execute") => json!({
            "operation":"execute",
            "workflowId":payload["workflowId"],
            "status":payload["outcome"]["status"],
            "commandId":payload["outcome"]["commandId"],
        }),
        Some("recoveryStatus") => json!({
            "operation":"recoveryStatus",
            "checkpointId":result["checkpoint"]["checkpointId"],
            "receipts":result["receipts"].as_array().map_or(0, Vec::len),
        }),
        Some("workflowRecover") => json!({
            "operation":"workflowRecover",
            "status":result["status"],
            "checkpointId":result["checkpointId"],
        }),
        Some("checkpointSave") => json!({
            "operation":"checkpointSave",
            "checkpointId":result["checkpointId"],
            "evidence":result["evidence"].as_array().map_or(0, Vec::len),
        }),
        _ => payload.clone(),
    }
}

async fn mcp_call(server: &Server, id: u64, name: &str, arguments: Value) -> Value {
    let response = server
        .handle_message(json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                               "params":{"name":name,"arguments":arguments}}))
        .await
        .expect("tools/call answers");
    assert!(response.get("error").is_none(), "{name}: {response}");
    response["result"]["structuredContent"].clone()
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn acp_walkthrough_matches_mcp_for_context_and_recovery() {
    let harness = ChromeRuntimeHarness::start().await;
    let site_url = harness.site_url();
    // Managed Chromium with a context store, as `bobby install` configures it:
    // the verified fill is remembered, so the context question has an answer.
    // The harness's own runtime holds the journal's ledger; release it first.
    let context = tempfile::tempdir().unwrap();
    let mut config = harness.config.clone();
    config.context.dir = Some(context.path().to_path_buf());
    drop(harness.runtime);
    drop(harness.service);
    let service = RuntimeService::build_with_context_promotion(
        &config,
        Arc::new(ChromiumWorkerFactory::new(config.browser.clone())),
        "managed-chromium",
    )
    .await
    .unwrap();
    let token = harness
        .authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            CAPABILITIES,
            Utc::now() + Duration::minutes(10),
        )
        .await
        .unwrap()
        .expose_once();
    let handle = harness.authority.verify(&token).await.unwrap();
    let runtime = Arc::new(AuthenticatedRuntime::new(service.clone(), handle));

    let (editor_io, agent_io) = tokio::io::duplex(1 << 20);
    let (agent_read, agent_write) = tokio::io::split(agent_io);
    let acp = AcpServer::new(Arc::clone(&runtime), CAPABILITIES.to_vec());
    tokio::spawn(acp.serve_io(agent_read, agent_write));
    let (editor_read, editor_write) = tokio::io::split(editor_io);
    let mut editor = Editor {
        writer: editor_write,
        lines: BufReader::new(editor_read).lines(),
        next_id: 0,
    };

    println!("editor -> initialize {{\"protocolVersion\":1}}");
    let (initialized, _) = editor
        .send(
            "initialize",
            json!({"protocolVersion":1,"clientCapabilities":{}}),
        )
        .await;
    println!("bobby  <- {initialized}");
    let (session, _) = editor
        .send("session/new", json!({"cwd":"/tmp","mcpServers":[]}))
        .await;
    let session_id = session["sessionId"].as_str().unwrap().to_owned();
    println!("editor -> session/new ; bobby <- sessionId {session_id}");

    let workflow_id = uuid::Uuid::new_v4().to_string();
    let filled = editor
        .prompt(
            &session_id,
            json!({
                "url": site_url.clone(),
                "workflowId": workflow_id,
                "intent": {"kind":"fill","input":{
                    "purpose":"Name",
                    "hints":{"role":"textbox","accessibleName":"Name"},
                    "value":{"kind":"setText","value":"Maya Chen"}
                }}
            }),
        )
        .await;
    assert_eq!(filled["operation"], "execute");
    assert_eq!(filled["outcome"]["status"], "completed", "{filled}");
    assert_eq!(filled["workflowId"], workflow_id);
    let page_id = filled["pageId"].as_str().unwrap().to_owned();
    let command_id = filled["outcome"]["commandId"].clone();

    let asked = editor
        .prompt(
            &session_id,
            json!({"operation":"contextAsk","description":"Name"}),
        )
        .await;
    assert_eq!(asked["operation"], "contextAsk");
    assert_eq!(asked["result"]["hit"], true, "{asked}");
    assert_eq!(asked["result"]["pageDerived"], true, "{asked}");

    let checkpoint_id = uuid::Uuid::new_v4().to_string();
    let saved = editor
        .prompt(
            &session_id,
            json!({
                "operation":"checkpointSave",
                "checkpoint":{
                    "schemaVersion":1,
                    "checkpointId":checkpoint_id,
                    "workflowId":workflow_id,
                    "attemptId":filled["attemptId"],
                    "sessionId":session_id,
                    "pageId":page_id,
                    "restartUrl":site_url.clone(),
                    "currentUrl":site_url.clone(),
                    "cursor":null,
                    "boundaryCommandId":null,
                    "recoveryClass":"replayable",
                    "invariants":[],
                    "replayableInputs":[],
                    "evidence":[],
                    "recoveryHistory":[],
                    "recoveryReceipts":[],
                    "createdAt":Utc::now().to_rfc3339(),
                },
                "evidenceRefs":[command_id],
            }),
        )
        .await;
    assert_eq!(saved["result"]["checkpointId"], checkpoint_id, "{saved}");
    assert!(
        !saved["result"]["evidence"].as_array().unwrap().is_empty(),
        "the fill's evidence is resolved into the checkpoint: {saved}"
    );

    let status = editor
        .prompt(
            &session_id,
            json!({"operation":"recoveryStatus","workflowId":workflow_id}),
        )
        .await;
    assert_eq!(
        status["result"]["checkpoint"]["checkpointId"],
        checkpoint_id
    );

    // MCP, same runtime and principal: the same questions, the same answers.
    let mcp = Server::new(Arc::clone(&runtime));
    mcp.handle_message(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
        "params":{"protocolVersion":"2025-11-25","capabilities":{},
                  "clientInfo":{"name":"acp-walkthrough","version":"1"}}}))
        .await
        .unwrap();
    mcp.handle_message(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .await;
    let mcp_asked = mcp_call(
        &mcp,
        2,
        "context_ask",
        json!({"sessionId":session_id,"pageId":page_id,"description":"Name"}),
    )
    .await;
    assert_eq!(mcp_asked, asked["result"], "context_ask parity");
    let mcp_status = mcp_call(
        &mcp,
        3,
        "recovery_status",
        json!({"workflowId":workflow_id}),
    )
    .await;
    assert_eq!(mcp_status, status["result"], "recovery_status parity");
    println!("mcp    -> context_ask, recovery_status ; same answers as ACP");

    let recovered = editor
        .prompt(
            &session_id,
            json!({"operation":"workflowRecover","workflowId":workflow_id}),
        )
        .await;
    assert_eq!(recovered["result"]["status"], "resumed", "{recovered}");
    assert_eq!(recovered["result"]["checkpointId"], checkpoint_id);

    let (closed, _) = editor
        .send("session/close", json!({"sessionId":session_id}))
        .await;
    println!("editor -> session/close ; bobby <- {closed}");
    assert!(service.list_sessions().await.is_empty());
}
