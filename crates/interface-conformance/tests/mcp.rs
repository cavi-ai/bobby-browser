use std::sync::Arc;

use interface_core::AuthorityStore;
use mcp_gateway::Server;
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use types::{Capability, PrincipalId};

#[tokio::test]
async fn mcp_server_observes_live_capability_filtered_json_rpc_boundary() {
    let authority = AuthorityStore::in_memory();
    let credential = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [Capability::SessionRead],
            chrono::Utc::now() + chrono::Duration::minutes(5),
        )
        .await
        .unwrap();
    let handle = authority.verify(&credential.expose_once()).await.unwrap();
    let server = Server::new(Arc::new(AuthenticatedRuntime::new(
        RuntimeService::default(),
        handle,
    )));
    let initialized = server.handle_message(json!({
        "jsonrpc":"2.0","id":1,"method":"initialize",
        "params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"conformance","version":"1"}}
    })).await.unwrap();
    assert_eq!(initialized["result"]["protocolVersion"], "2025-11-25");
    assert!(server
        .handle_message(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .await
        .is_none());
    let listed = server
        .handle_message(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
        .await
        .unwrap();
    let names = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"runtime_info"));
    assert!(!names.contains(&"session_create"));
    let denied = server.handle_message(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"session_create","arguments":{"profile":"denied","proxy":Value::Null}}})).await.unwrap();
    assert!(denied.get("error").is_some());
}
