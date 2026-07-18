use std::sync::Arc;

use chrono::{Duration, Utc};
use interface_core::{AuthorityStore, CapabilityHandle};
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
    Server::new(runtime, handle)
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
