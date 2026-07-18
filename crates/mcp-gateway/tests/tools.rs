use std::sync::Arc;

use chrono::{Duration, Utc};
use interface_core::{AuthorityStore, Event, EventStore};
use mcp_gateway::{ArtifactResources, Server};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use types::{Capability, PrincipalId};
use uuid::uuid;

async fn fixture_server(capabilities: Vec<Capability>) -> Server {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000016")),
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
    let server = Server::new(runtime, handle);
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
                "clientInfo":{"name":"test","version":"1"}
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

#[tokio::test]
async fn tools_are_capability_filtered_sorted_and_have_closed_schemas() {
    let server = fixture_server(vec![Capability::SessionRead, Capability::PageWrite]).await;
    let response = server
        .handle_message(request(2, "tools/list", json!({})))
        .await
        .unwrap();
    let tools = response["result"]["tools"].as_array().unwrap();
    let names = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        ["events_read", "page_open", "runtime_info", "session_list"]
    );
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        assert!(tool["inputSchema"]["properties"].is_object());
    }
}

#[tokio::test]
async fn runtime_info_calls_the_authenticated_runtime_and_returns_structured_content() {
    let server = fixture_server(vec![Capability::SessionRead]).await;
    let response = server
        .handle_message(request(
            3,
            "tools/call",
            json!({"name":"runtime_info","arguments":{}}),
        ))
        .await
        .unwrap();

    assert_eq!(response["id"], 3);
    assert_eq!(response["result"]["isError"], false);
    assert_eq!(
        response["result"]["structuredContent"]["active_sessions"],
        0
    );
    assert_eq!(response["result"]["content"][0]["type"], "text");
}

#[tokio::test]
async fn unavailable_or_malformed_tool_calls_fail_without_dispatch() {
    let server = fixture_server(vec![Capability::SessionRead]).await;
    let unavailable = server
        .handle_message(request(
            4,
            "tools/call",
            json!({"name":"command_execute","arguments":{}}),
        ))
        .await
        .unwrap();
    assert_eq!(unavailable["error"]["code"], -32601);

    let malformed = server
        .handle_message(request(
            5,
            "tools/call",
            json!({"name":"runtime_info","arguments":{"bearer":"do-not-accept"}}),
        ))
        .await
        .unwrap();
    assert_eq!(malformed["error"]["code"], -32602);
}

#[tokio::test]
async fn events_read_preserves_exact_event_gap_metadata() {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000017")),
            [Capability::SessionRead],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let runtime = Arc::new(AuthenticatedRuntime::new(
        RuntimeService::default(),
        handle.clone(),
    ));
    let events = EventStore::new(1);
    events.append(Event::new("one", json!({}))).await;
    events.append(Event::new("two", json!({}))).await;
    let server = Server::new(runtime, handle).with_boundaries(events, ArtifactResources::default());
    initialize(&server).await;

    let response = server
        .handle_message(request(
            20,
            "tools/call",
            json!({"name":"events_read","arguments":{"cursor":0,"limit":1}}),
        ))
        .await
        .unwrap();
    assert_eq!(response["error"]["code"], -32000);
    assert_eq!(
        response["error"]["data"]["eventGap"]["reason"],
        "historyLost"
    );
    assert_eq!(
        response["error"]["data"]["eventGap"]["earliestAvailable"],
        2
    );
}
