use std::sync::Arc;

use chrono::{Duration, Utc};
use interface_core::{AuthorityStore, CapabilityHandle};
use mcp_gateway::protocol::{MAX_FRAME_BYTES, MAX_REQUEST_ID_BYTES};
use mcp_gateway::Server;
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use types::{Capability, PrincipalId};
use uuid::uuid;

async fn fixture_server(capabilities: Vec<Capability>) -> Server {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000006")),
            capabilities,
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle: CapabilityHandle = authority.verify(&token.expose_once()).await.unwrap();
    let runtime = Arc::new(AuthenticatedRuntime::new(
        RuntimeService::default(),
        handle.clone(),
    ));
    Server::new(runtime)
}

fn request(id: Value, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}

#[tokio::test]
async fn initialize_must_be_first_and_negotiates_2025_11_25() {
    let server = fixture_server(vec![Capability::SessionRead]).await;

    let denied = server
        .handle_message(request(json!(1), "tools/list", json!({})))
        .await
        .unwrap();
    assert_eq!(denied["error"]["code"], -32002);

    let initialized = server
        .handle_message(request(
            json!(2),
            "initialize",
            json!({
                "protocolVersion":"2025-11-25",
                "capabilities":{},
                "clientInfo":{"name":"official-client","version":"1.0.0"}
            }),
        ))
        .await
        .unwrap();
    assert_eq!(initialized["id"], 2);
    assert_eq!(initialized["result"]["protocolVersion"], "2025-11-25");

    let still_gated = server
        .handle_message(request(json!(3), "tools/list", json!({})))
        .await
        .unwrap();
    assert_eq!(still_gated["error"]["code"], -32002);

    assert!(server
        .handle_message(json!({
            "jsonrpc":"2.0",
            "method":"notifications/initialized",
            "params":{}
        }))
        .await
        .is_none());
    let tools = server
        .handle_message(request(json!(4), "tools/list", json!({})))
        .await
        .unwrap();
    assert!(tools["result"]["tools"].is_array());
}

#[tokio::test]
async fn rejects_batches_wrong_jsonrpc_and_duplicate_initialize_without_losing_ids() {
    let server = fixture_server(vec![Capability::SessionRead]).await;

    assert_eq!(
        server.handle_message(json!([])).await.unwrap()["error"]["code"],
        -32600
    );
    let wrong_version = server
        .handle_message(json!({"jsonrpc":"1.0","id":"kept","method":"initialize","params":{}}))
        .await
        .unwrap();
    assert_eq!(wrong_version["id"], "kept");
    assert_eq!(wrong_version["error"]["code"], -32600);

    let first = server
        .handle_message(request(
            json!(9),
            "initialize",
            json!({
                "protocolVersion":"2025-11-25",
                "capabilities":{},
                "clientInfo":{"name":"test","version":"1"}
            }),
        ))
        .await
        .unwrap();
    assert!(first.get("result").is_some());
    let duplicate = server
        .handle_message(request(
            json!(10),
            "initialize",
            json!({
                "protocolVersion":"2025-11-25",
                "capabilities":{},
                "clientInfo":{"name":"test","version":"1"}
            }),
        ))
        .await
        .unwrap();
    assert_eq!(duplicate["id"], 10);
    assert_eq!(duplicate["error"]["code"], -32600);
}

#[tokio::test]
async fn stdio_is_one_bounded_object_per_line_and_eof_is_clean() {
    let server = fixture_server(vec![Capability::SessionRead]).await;
    let input = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"clientInfo\":{\"name\":\"test\",\"version\":\"1\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\",\"params\":{}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{}}\n"
    );
    let mut output = Vec::new();
    server
        .serve(input.as_bytes(), &mut output)
        .await
        .expect("graceful EOF");

    let stdout = String::from_utf8(output).unwrap();
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2, "notifications produce no stdout: {stdout}");
    for line in lines {
        let message: Value = serde_json::from_str(line).unwrap();
        assert_eq!(message["jsonrpc"], "2.0");
    }
}

#[tokio::test]
async fn initialize_rejects_non_object_and_oversized_capabilities_without_advancing() {
    for capabilities in [
        json!("invalid"),
        json!([]),
        json!({"experimental":{"x":"x".repeat(20 * 1024)}}),
    ] {
        let server = fixture_server(vec![Capability::SessionRead]).await;
        let rejected = server
            .handle_message(request(
                json!(1),
                "initialize",
                json!({
                    "protocolVersion":"2025-11-25",
                    "capabilities":capabilities.clone(),
                    "clientInfo":{"name":"test","version":"1"}
                }),
            ))
            .await
            .unwrap();
        assert_eq!(
            rejected["error"]["code"], -32602,
            "capabilities={capabilities}; response={rejected}"
        );
        let accepted = server
            .handle_message(request(
                json!(2),
                "initialize",
                json!({
                    "protocolVersion":"2025-11-25","capabilities":{},
                    "clientInfo":{"name":"test","version":"1"}
                }),
            ))
            .await
            .unwrap();
        assert!(accepted.get("result").is_some(), "{accepted}");
    }
}

#[tokio::test]
async fn oversized_request_ids_are_replaced_by_null_and_every_stdout_line_is_bounded() {
    let server = fixture_server(vec![Capability::SessionRead]).await;
    initialize_ready(&server).await;
    let huge_id = "i".repeat(MAX_REQUEST_ID_BYTES + 1);
    let response = server
        .handle_message(request(json!(huge_id), "ping", json!({})))
        .await
        .unwrap();
    assert!(response["id"].is_null());

    let empty = request(json!(3), "ping", json!({"padding":""})).to_string();
    let exact_payload = "x".repeat(MAX_FRAME_BYTES - empty.len());
    let frame = request(json!(3), "ping", json!({"padding":exact_payload})).to_string();
    assert_eq!(frame.len(), MAX_FRAME_BYTES);
    let input = format!("{frame}\n");
    let mut output = Vec::new();
    server.serve(input.as_bytes(), &mut output).await.unwrap();
    for line in output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        assert!(line.len() <= MAX_FRAME_BYTES);
        serde_json::from_slice::<Value>(line).unwrap();
    }
}

#[tokio::test]
async fn serve_starts_waiting_work_before_cancel_and_eof_drains_promptly() {
    let cancelled = fixture_server(vec![Capability::SessionRead]).await;
    initialize_ready(&cancelled).await;
    let cancel_input = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":77,\"method\":\"tools/call\",\"params\":{\"name\":\"events_read\",\"arguments\":{\"cursor\":0,\"limit\":1}}}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":77}}\n"
    );
    let mut output = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_millis(500),
        cancelled.serve(cancel_input.as_bytes(), &mut output),
    )
    .await
    .unwrap()
    .unwrap();
    let lines = String::from_utf8(output).unwrap();
    for line in lines.lines() {
        serde_json::from_str::<Value>(line).unwrap();
    }
    assert!(lines.contains("-32800"), "{lines}");

    let eof = fixture_server(vec![Capability::SessionRead]).await;
    initialize_ready(&eof).await;
    let eof_input = "{\"jsonrpc\":\"2.0\",\"id\":78,\"method\":\"tools/call\",\"params\":{\"name\":\"events_read\",\"arguments\":{\"cursor\":0,\"limit\":1}}}\n";
    let mut eof_output = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_millis(500),
        eof.serve(eof_input.as_bytes(), &mut eof_output),
    )
    .await
    .unwrap()
    .unwrap();
    for line in eof_output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        serde_json::from_slice::<Value>(line).unwrap();
    }
}

async fn initialize_ready(server: &Server) {
    server
        .handle_message(request(
            json!(90),
            "initialize",
            json!({
                "protocolVersion":"2025-11-25","capabilities":{},
                "clientInfo":{"name":"test","version":"1"}
            }),
        ))
        .await;
    server
        .handle_message(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .await;
}
