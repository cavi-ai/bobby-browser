//! Later tasks in the mcp-surface-depth plan append tests to this file and
//! reuse `all_capabilities` / `list_tools`, so keep them `pub` even before
//! those tasks land.
#![allow(dead_code)]

use std::sync::Arc;

use chrono::{Duration, Utc};
use interface_core::AuthorityStore;
use mcp_gateway::Server;
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use types::{
    AttemptId, Capability, CommandEnvelope, CommandId, IntentCommand, IntentHints, LocateIntent,
    PrincipalId, RuntimeCommand, SessionId, WorkflowId,
};
use uuid::uuid;

/// The `tools/list` payload an agent downloads on connect, in bytes.
///
/// Measured at 105,800 on `6ba4a15`. Three merges on a single day added
/// 8,131 bytes without any reviewer seeing the number, which is what this
/// gate exists to stop.
const TOOLS_LIST_MAX_BYTES: usize = 160_000;

pub fn all_capabilities() -> Vec<Capability> {
    vec![
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
        Capability::ArtifactCapture,
        Capability::RecoveryRead,
        Capability::RecoveryWrite,
        Capability::AuthorityAdmin,
    ]
}

async fn fixture_server(capabilities: Vec<Capability>) -> Server {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000032")),
            capabilities,
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let runtime = Arc::new(AuthenticatedRuntime::new(
        RuntimeService::default(),
        handle.clone(),
    ));
    let server = Server::new(runtime);
    initialize(&server).await;
    server
}

async fn initialize(server: &Server) {
    server
        .handle_message(request(
            1,
            "initialize",
            json!({
                "protocolVersion":"2025-11-25",
                "capabilities":{},
                "clientInfo":{"name":"budget","version":"1"}
            }),
        ))
        .await;
    server
        .handle_message(json!({
            "jsonrpc":"2.0","method":"notifications/initialized","params":{}
        }))
        .await;
}

fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}

pub async fn list_tools(capabilities: Vec<Capability>) -> Vec<Value> {
    let server = fixture_server(capabilities).await;
    let response = server
        .handle_message(request(2, "tools/list", json!({})))
        .await
        .expect("tools/list returns a response");
    response["result"]["tools"]
        .as_array()
        .expect("tools is an array")
        .clone()
}

#[tokio::test]
async fn tools_list_stays_within_the_connect_budget() {
    let tools = list_tools(all_capabilities()).await;
    let bytes = serde_json::to_string(&tools).unwrap().len();
    assert!(
        bytes <= TOOLS_LIST_MAX_BYTES,
        "tools/list is {bytes} bytes, over the {TOOLS_LIST_MAX_BYTES} byte budget"
    );
}

#[tokio::test]
async fn tools_list_never_exceeds_the_frame_cap() {
    let tools = list_tools(all_capabilities()).await;
    let bytes = serde_json::to_string(&tools).unwrap().len();
    assert!(bytes < 1024 * 1024, "tools/list would exceed the frame cap");
}

#[tokio::test]
async fn every_advertised_tool_carries_a_name_and_input_schema() {
    for tool in list_tools(all_capabilities()).await {
        assert!(tool["name"].is_string(), "tool without a name: {tool}");
        assert!(
            tool["inputSchema"].is_object(),
            "tool without an inputSchema: {}",
            tool["name"]
        );
    }
}

#[tokio::test]
async fn command_execute_does_not_advertise_the_command_union() {
    let tools = list_tools(all_capabilities()).await;
    let tool = tools
        .iter()
        .find(|tool| tool["name"] == "command_execute")
        .expect("command_execute is advertised");
    let defs = &tool["inputSchema"]["$defs"];
    assert!(
        defs.get("PrimitiveCommand").is_none(),
        "command_execute still carries PrimitiveCommand"
    );
    assert!(
        defs.get("IntentCommand").is_none(),
        "command_execute still carries IntentCommand"
    );
    let bytes = serde_json::to_string(tool).unwrap().len();
    assert!(
        bytes < 2_000,
        "command_execute is {bytes} bytes, expected under 2000"
    );
}

#[tokio::test]
async fn command_execute_points_agents_at_the_named_tools() {
    let tools = list_tools(all_capabilities()).await;
    let tool = tools
        .iter()
        .find(|tool| tool["name"] == "command_execute")
        .expect("command_execute is advertised");
    let command = &tool["inputSchema"]["$defs"]["CommandEnvelope"]["properties"]["command"];
    assert_eq!(command["type"], "object");
    let description = command["description"]
        .as_str()
        .expect("the opaque command carries a description");
    assert!(
        description.contains("intent_") || description.contains("named tool"),
        "description does not point at the named tools: {description}"
    );
}

#[tokio::test]
async fn narrowing_the_advertised_schema_did_not_narrow_validation() {
    // `command_execute` advertises an opaque command, but still rejects
    // malformed nested command content at the MCP edge rather than
    // dispatching it into the runtime. Envelope shape and corruption copied
    // from `command_execute_schema_rejects_locate_purpose_over_256` in
    // `tests/tools.rs`, which proved this -32602 pre-dispatch rejection
    // before the advertised schema was narrowed.
    let server = fixture_server(all_capabilities()).await;
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: None,
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Intent(IntentCommand::Locate(LocateIntent {
            purpose: "Continue".to_owned(),
            hints: IntentHints::default(),
        })),
    };
    let mut envelope_value = serde_json::to_value(envelope).unwrap();
    envelope_value["command"]["input"]["input"]["purpose"] = json!("a".repeat(257));

    let response = server
        .handle_message(request(
            5,
            "tools/call",
            json!({
                "name":"command_execute",
                "arguments":{"envelope":envelope_value}
            }),
        ))
        .await
        .expect("a response");
    assert_eq!(response["error"]["code"], -32602, "{response}");
}

#[tokio::test]
async fn checkpoint_save_does_not_advertise_the_evidence_union() {
    let tools = list_tools(all_capabilities()).await;
    let tool = tools
        .iter()
        .find(|tool| tool["name"] == "checkpoint_save")
        .expect("checkpoint_save is advertised");
    assert!(
        tool["inputSchema"]["$defs"].get("Evidence").is_none(),
        "checkpoint_save still carries the Evidence union"
    );
    assert!(
        tool["inputSchema"]["properties"]["evidenceRefs"].is_object(),
        "checkpoint_save does not accept evidenceRefs"
    );
    let bytes = serde_json::to_string(tool).unwrap().len();
    assert!(
        bytes < 13_000,
        "checkpoint_save is {bytes} bytes, expected under 13000"
    );
}

const READ_ONLY: &[&str] = &[
    "runtime_info",
    "session_list",
    "page_list",
    "inspect",
    "a11y_snapshot",
    "form_snapshot",
    "screenshot",
    "events_read",
    "recovery_status",
    "cookie_get",
];

const DESTRUCTIVE: &[&str] = &["session_close", "page_close", "cookie_delete"];

// `command_execute` accepts an arbitrary `RuntimeCommand`, which can itself be
// `Navigate` or `DownloadUrl` — envelope-mediated navigation reaches the
// network exactly like the standalone tools below.
const OPEN_WORLD: &[&str] = &[
    "navigate",
    "download_url",
    "extract_structured",
    "command_execute",
];

// `idempotentHint` is unconditional under MCP: repeating the call with the
// same arguments must have no additional effect. An optional
// `idempotencyKey` does not establish that (without one, `session_create`
// mints a second session and `intent_fill`/`command_execute` have no dedupe
// at all), so only tools that converge regardless of any key belong here.
const IDEMPOTENT: &[&str] = &["checkpoint_save", "emulate"];

/// `tool_title`'s wildcard arm (`annotations.rs`) returns this exact string
/// for any name with no explicit arm. It has to compile as a total match, but
/// it must never actually fire for an advertised tool — see
/// `every_tool_carries_a_title_and_annotations` below.
const UNTITLED_FALLBACK: &str = "Untitled tool";

#[tokio::test]
async fn every_tool_carries_a_title_and_annotations() {
    for tool in list_tools(all_capabilities()).await {
        let name = tool["name"].as_str().unwrap().to_owned();
        assert!(tool["title"].is_string(), "{name} has no title");
        assert_ne!(
            tool["title"],
            serde_json::json!(UNTITLED_FALLBACK),
            "{name} fell through to tool_title's fallback arm — add a real title in annotations.rs"
        );
        assert!(tool["annotations"].is_object(), "{name} has no annotations");
    }
}

#[tokio::test]
async fn idempotent_hints_match_the_spec_table() {
    for tool in list_tools(all_capabilities()).await {
        let name = tool["name"].as_str().unwrap();
        assert_eq!(
            tool["annotations"]["idempotentHint"] == serde_json::json!(true),
            IDEMPOTENT.contains(&name),
            "{name} idempotentHint disagrees with the spec table"
        );
    }
}

#[tokio::test]
async fn read_only_tools_are_marked_read_only() {
    let tools = list_tools(all_capabilities()).await;
    for tool in &tools {
        let name = tool["name"].as_str().unwrap();
        let read_only = tool["annotations"]["readOnlyHint"] == serde_json::json!(true);
        assert_eq!(
            read_only,
            READ_ONLY.contains(&name),
            "{name} readOnlyHint disagrees with the spec table"
        );
    }
}

#[tokio::test]
async fn destructive_and_open_world_hints_match_the_spec_table() {
    for tool in list_tools(all_capabilities()).await {
        let name = tool["name"].as_str().unwrap();
        assert_eq!(
            tool["annotations"]["destructiveHint"] == serde_json::json!(true),
            DESTRUCTIVE.contains(&name),
            "{name} destructiveHint disagrees with the spec table"
        );
        assert_eq!(
            tool["annotations"]["openWorldHint"] == serde_json::json!(true),
            OPEN_WORLD.contains(&name),
            "{name} openWorldHint disagrees with the spec table"
        );
    }
}

#[tokio::test]
async fn a_read_only_tool_is_never_also_destructive() {
    for tool in list_tools(all_capabilities()).await {
        let annotations = &tool["annotations"];
        let read_only = annotations["readOnlyHint"] == serde_json::json!(true);
        let destructive = annotations["destructiveHint"] == serde_json::json!(true);
        assert!(
            !(read_only && destructive),
            "{} is both read-only and destructive",
            tool["name"]
        );
    }
}
