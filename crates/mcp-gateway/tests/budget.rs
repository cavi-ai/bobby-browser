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
use types::{Capability, PrincipalId};
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
