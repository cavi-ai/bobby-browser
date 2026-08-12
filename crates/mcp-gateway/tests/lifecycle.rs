use std::sync::Arc;

use chrono::{Duration, Utc};
use interface_core::{AuthorityStore, CapabilityHandle, EventStore, RuntimeInterface};
use mcp_gateway::protocol::{MAX_FRAME_BYTES, MAX_REQUEST_ID_BYTES};
use mcp_gateway::{ArtifactResources, Server};
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
async fn rejects_batches_and_wrong_jsonrpc_while_reinitialize_resets_without_losing_ids() {
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
    // A re-`initialize` is a session reset (not -32600): reconnecting
    // streamable-HTTP clients call `initialize` on every connect.
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
    assert!(duplicate.get("error").is_none(), "{duplicate}");
    assert!(duplicate["result"]["protocolVersion"].is_string());
    // The lifecycle genuinely reset: traffic before the new handshake
    // completes is gated as not-initialized.
    let gated = server
        .handle_message(request(json!(11), "tools/list", json!({})))
        .await
        .unwrap();
    assert_eq!(gated["error"]["code"], -32002);
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

/// Clients often write `notifications/initialized` and the next request without
/// waiting. Those frames must not be handled concurrently, or the request
/// observes `AwaitingInitializedNotification` and returns -32002.
#[tokio::test]
async fn stdio_initialized_then_next_request_does_not_race_to_not_initialized() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    for _ in 0..64 {
        let server = fixture_server(vec![Capability::SessionRead]).await;
        let (client, server_io) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server_io);
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut reader = BufReader::new(client_read);

        let driver = async {
            // One write keeps initialized + tools/list adjacent on the wire —
            // the race the concurrent pending dispatch used to lose.
            client_write
                .write_all(
                    concat!(
                        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"clientInfo\":{\"name\":\"race\",\"version\":\"1\"}}}\n",
                        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\",\"params\":{}}\n",
                        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{}}\n"
                    )
                    .as_bytes(),
                )
                .await
                .expect("handshake writes");

            let mut lines = Vec::new();
            while lines.len() < 2 {
                let mut line = String::new();
                reader.read_line(&mut line).await.expect("stdout reads");
                lines.push(serde_json::from_str::<Value>(line.trim()).expect("json line"));
            }
            drop(client_write);
            lines
        };

        let lines = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::select! {
                result = server.serve(server_read, server_write) => {
                    panic!("serve returned before handshake finished: {result:?}")
                }
                lines = driver => lines,
            }
        })
        .await
        .expect("handshake timed out");

        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].get("result").is_some(), "{}", lines[0]);
        assert_eq!(lines[1]["id"], 2, "{}", lines[1]);
        assert_ne!(
            lines[1].pointer("/error/code"),
            Some(&json!(-32002)),
            "initialized/tools/list raced: {}",
            lines[1]
        );
        assert!(lines[1]["result"]["tools"].is_array(), "{}", lines[1]);
    }
}

#[tokio::test]
async fn initialize_rejects_non_object_and_oversized_capabilities_without_advancing() {
    for capabilities in [
        json!("invalid"),
        json!([]),
        json!({"experimental":{"x":"x".repeat(20 * 1024)}}),
        json!({"experimental":{"scalar":true}}),
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
async fn initialize_accepts_official_2025_11_25_client_capabilities_and_extensions() {
    let server = fixture_server(vec![Capability::SessionRead]).await;
    let response = server
        .handle_message(request(
            json!(70),
            "initialize",
            json!({
                "protocolVersion":"2025-11-25",
                "capabilities":{
                    "roots":{"listChanged":true},
                    "sampling":{"context":{},"tools":{}},
                    "elicitation":{"form":{},"url":{}},
                    "tasks":{
                        "list":{},"cancel":{},
                        "requests":{
                            "sampling":{"createMessage":{}},
                            "elicitation":{"create":{}}
                        }
                    },
                    "experimental":{"com.example/feature":{"enabled":true}},
                    "com.example/custom":{"version":1}
                },
                "clientInfo":{"name":"official-client","version":"1"}
            }),
        ))
        .await
        .unwrap();
    assert!(response.get("result").is_some(), "{response}");
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

#[tokio::test]
async fn for_interface_constructor_serves_initialize() {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000006")),
            vec![Capability::SessionRead],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle: CapabilityHandle = authority.verify(&token.expose_once()).await.unwrap();
    let runtime: Arc<dyn RuntimeInterface> = Arc::new(AuthenticatedRuntime::new(
        RuntimeService::default(),
        handle.clone(),
    ));
    let server = Server::for_interface(
        runtime,
        handle,
        EventStore::new(16),
        ArtifactResources::default(),
    );

    let initialized = server
        .handle_message(request(
            json!(1),
            "initialize",
            json!({
                "protocolVersion":"2025-11-25",
                "capabilities":{},
                "clientInfo":{"name":"test","version":"1"}
            }),
        ))
        .await
        .unwrap();
    assert_eq!(initialized["result"]["protocolVersion"], "2025-11-25");
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

/// An older revision this gateway speaks is answered with that same revision, and the
/// session proceeds. Rejecting it with -32602 made the gateway unreachable from any host
/// pinned to an older revision — the client dropped the connection and bobby-browser never
/// appeared in its tool list, while `bobby doctor` (which asks for the newest) reported
/// the gateway healthy.
#[tokio::test]
async fn initialize_negotiates_an_older_supported_revision() {
    for requested in ["2025-06-18", "2025-03-26", "2024-11-05"] {
        let server = fixture_server(vec![Capability::SessionRead]).await;
        let initialized = server
            .handle_message(request(
                json!(1),
                "initialize",
                json!({
                    "protocolVersion": requested,
                    "capabilities":{},
                    "clientInfo":{"name":"older-client","version":"1.0.0"}
                }),
            ))
            .await
            .unwrap();
        assert!(
            initialized.get("error").is_none(),
            "{requested} was rejected: {initialized}"
        );
        assert_eq!(initialized["result"]["protocolVersion"], requested);

        server
            .handle_message(
                json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
            )
            .await;
        let tools = server
            .handle_message(request(json!(2), "tools/list", json!({})))
            .await
            .unwrap();
        assert!(
            tools["result"]["tools"].is_array(),
            "{requested} lost tools"
        );
    }
}

/// A revision this gateway does not implement is answered with the newest one it does,
/// which the spec leaves the client to accept or drop — not an error frame.
#[tokio::test]
async fn initialize_offers_the_newest_revision_when_there_is_no_overlap() {
    let server = fixture_server(vec![Capability::SessionRead]).await;
    let initialized = server
        .handle_message(request(
            json!(1),
            "initialize",
            json!({
                "protocolVersion":"1999-01-01",
                "capabilities":{},
                "clientInfo":{"name":"unknown-client","version":"1.0.0"}
            }),
        ))
        .await
        .unwrap();
    assert!(initialized.get("error").is_none());
    assert_eq!(initialized["result"]["protocolVersion"], "2025-11-25");
}
