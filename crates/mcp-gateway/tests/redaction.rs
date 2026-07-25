use std::sync::Arc;

use chrono::{Duration, Utc};
use interface_core::AuthorityStore;
use mcp_gateway::{protocol::MAX_FRAME_BYTES, Server};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use types::{Capability, PrincipalId};
use uuid::uuid;

const SECRET: &str = "planted-bearer-secret-000000000000000000000000000000";

async fn fixture() -> Server {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000036")),
            [Capability::SessionRead],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let server = Server::new(Arc::new(AuthenticatedRuntime::new(
        RuntimeService::default(),
        handle,
    )));
    initialize(&server).await;
    server
}

async fn initialize(server: &Server) {
    server
        .handle_message(request(
            1,
            "initialize",
            json!({
                "protocolVersion":"2025-11-25","capabilities":{},
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
async fn malformed_oversized_and_cancelled_inputs_never_pollute_stdout() {
    let server = fixture().await;
    let oversized = format!(
        "{{\"secret\":\"{SECRET}{}\"}}\n",
        "x".repeat(MAX_FRAME_BYTES)
    );
    let input = format!(
        "{{bad {SECRET}}}\n{oversized}{{\"jsonrpc\":\"2.0\",\"id\":99,\"method\":\"unknown-{SECRET}\",\"params\":{{}}}}\n"
    );
    let mut stdout = Vec::new();
    server.serve(input.as_bytes(), &mut stdout).await.unwrap();
    let stdout = String::from_utf8(stdout).unwrap();
    assert!(!stdout.contains(SECRET), "{stdout}");
    for line in stdout.lines() {
        let value: Value = serde_json::from_str(line).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
    }

    assert!(server
        .handle_message(json!({
            "jsonrpc":"2.0","method":"notifications/cancelled",
            "params":{"requestId":42,"reason":SECRET}
        }))
        .await
        .is_none());
}

#[tokio::test]
async fn cancellation_interrupts_a_pending_request_by_its_json_rpc_id() {
    let server = Arc::new(fixture().await);
    let pending_server = server.clone();
    let pending = tokio::spawn(async move {
        pending_server
            .handle_message(request(
                77,
                "tools/call",
                json!({"name":"events_read","arguments":{"cursor":0,"limit":1}}),
            ))
            .await
            .unwrap()
    });
    tokio::task::yield_now().await;
    let cancellation = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        server.handle_message(json!({
            "jsonrpc":"2.0","method":"notifications/cancelled",
            "params":{"requestId":77,"reason":"client no longer needs result"}
        })),
    )
    .await
    .expect("cancellation notification must not wait behind the operation");
    assert!(cancellation.is_none());
    let response = tokio::time::timeout(std::time::Duration::from_millis(100), pending)
        .await
        .expect("cancellation must promptly interrupt the pending operation")
        .unwrap();
    assert_eq!(response["error"]["code"], -32800);
}

#[tokio::test]
async fn cancellation_arriving_before_registration_is_not_lost() {
    let server = fixture().await;
    assert!(server
        .handle_message(json!({
            "jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":88}
        }))
        .await
        .is_none());
    let response = server
        .handle_message(request(
            88,
            "tools/call",
            json!({"name":"events_read","arguments":{"cursor":0,"limit":1}}),
        ))
        .await
        .unwrap();
    assert_eq!(response["error"]["code"], -32800);
}
