use chrono::{Duration, Utc};
use interface_core::AuthorityStore;
use mcp_gateway::Server;
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use std::sync::Arc;
use types::{Capability, PrincipalId};

async fn rpc(server: &Server, id: u64, method: &str, params: Value) -> Value {
    server
        .handle_message(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
        .await
        .unwrap()
}

#[tokio::test]
async fn session_capabilities_is_a_read_only_tool_with_a_validated_output() {
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [Capability::SessionRead, Capability::SessionWrite],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let handle = authority.verify(&token).await.unwrap();
    let server = Server::new(Arc::new(AuthenticatedRuntime::new(
        RuntimeService::default(),
        handle,
    )));
    rpc(&server, 1, "initialize", json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"capability-contract","version":"1"}})).await;
    server
        .handle_message(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .await;
    let created = rpc(
        &server,
        2,
        "tools/call",
        json!({"name":"session_create","arguments":{"profile":"capabilities"}}),
    )
    .await;
    let session = &created["result"]["structuredContent"]["id"];
    assert!(session.is_string(), "{created}");
    let response = rpc(
        &server,
        3,
        "tools/call",
        json!({"name":"session_capabilities","arguments":{"sessionId":session}}),
    )
    .await;
    let report: types::SessionCapabilities =
        serde_json::from_value(response["result"]["structuredContent"].clone())
            .unwrap_or_else(|_| panic!("capability response: {response}"));
    assert_eq!(serde_json::to_value(report.session_id).unwrap(), *session);
    assert!(report.engine.is_none());
    assert!(!report.features.iter().any(|f| f.available));
    let listed = rpc(&server, 4, "tools/list", json!({})).await;
    assert!(listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "session_capabilities"));
    let listed = rpc(
        &server,
        5,
        "tools/call",
        json!({"name":"session_list","arguments":{}}),
    )
    .await;
    assert_eq!(
        listed["result"]["structuredContent"]["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(listed["result"]["structuredContent"]["sessions"][0]
        .get("features")
        .is_none());
}
