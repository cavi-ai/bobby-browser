//! Multi-principal acceptance test over the production bootstrap path (`broker::serve`,
//! which wires `PersistentAuthority` and a real `RuntimeService::build`). It binds a real
//! loopback TCP listener and speaks HTTP/1.1 over it by hand, since this workspace has no
//! HTTP client dependency. Proves:
//!
//! - admin (`authority:admin`) issues two independently-capable principals over
//!   `POST /v1/principals`,
//! - both run concurrent, independent MCP lifecycles over `POST /v1/mcp`,
//! - a principal calling a tool gated behind a capability it was never issued gets a
//!   JSON-RPC error (and the other principal is unaffected),
//! - admin revocation (`DELETE /v1/principals/{id}`) takes effect immediately over
//!   `/v1/mcp` (401) while the untouched principal keeps working,
//! - and all of that survives a full process restart against the same
//!   `authority_path`: the still-valid bearer re-authenticates and gets a fresh MCP
//!   lifecycle, the revoked bearer stays revoked.

use std::{
    net::SocketAddr,
    path::Path,
    time::{Duration as StdDuration, Instant},
};

use broker::StartupCredential;
use chrono::{Duration, SecondsFormat, Utc};
use config::AppConfig;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    net::TcpStream,
    task::JoinHandle,
};
use types::{Capability, CURRENT_INTERFACE_VERSION};
use uuid::Uuid;

const BOOTSTRAP_BEARER: &str = "acceptance-bootstrap-bearer-0123456789abcdef01";

/// A live broker process (production `broker::serve` path) bound to a real loopback
/// port, plus the join handle for its serve task.
struct RunningBroker {
    address: SocketAddr,
    task: JoinHandle<()>,
}

impl RunningBroker {
    /// Aborts the serve task (dropping its bound `TcpListener` immediately) and waits
    /// for the task to actually finish, so a subsequent boot never races the previous
    /// process's shutdown.
    async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

/// Grabs a free loopback port by binding then immediately dropping a listener. `serve`
/// takes a config-driven `host:port` and never reports the bound address, so the port has
/// to be known before the call.
async fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    listener
        .local_addr()
        .expect("listener has local addr")
        .port()
}

/// Boots through the production path: `broker::serve`, which runs `bootstrap_listener_with`
/// (validated `StartupCredential`, `PersistentAuthority` wrapping `EnrolledAuthority` at
/// `authority_path`, real `RuntimeService::build`) on a bound socket.
async fn boot_production(
    authority_path: &Path,
    data_root: &Path,
    max_principals: usize,
    max_in_flight_per_principal: usize,
) -> RunningBroker {
    let port = free_port().await;
    let mut config = AppConfig::default();
    config.server.host = "127.0.0.1".to_owned();
    config.server.port = port;
    config.browser.profiles_dir = data_root.join("profiles");
    config.browser.upload_roots = vec![data_root.join("uploads")];
    config.browser.downloads_dir = data_root.join("downloads");
    config.browser.artifacts_dir = data_root.join("artifacts");
    config.storage.journal_path = data_root.join("commands.jsonl");
    config.storage.checkpoints_dir = data_root.join("checkpoints");
    config.storage.authority_path = authority_path.to_path_buf();
    config.interface.max_principals = max_principals;
    config.interface.max_in_flight_per_principal = max_in_flight_per_principal;
    for path in [
        &config.browser.upload_roots[0],
        &config.browser.downloads_dir,
        &config.browser.artifacts_dir,
        &config.storage.checkpoints_dir,
    ] {
        std::fs::create_dir_all(path).expect("create confined test directory");
    }

    let startup = StartupCredential::new(
        BOOTSTRAP_BEARER.to_owned(),
        types::PrincipalId::from_uuid(Uuid::nil()),
        vec![
            Capability::AuthorityAdmin,
            Capability::SessionRead,
            Capability::SessionWrite,
            Capability::PageRead,
            Capability::PageWrite,
            Capability::BrowserMutate,
            Capability::FileUpload,
        ],
        Utc::now() + Duration::minutes(30),
    )
    .expect("bootstrap startup credential is valid");

    let address: SocketAddr = format!("127.0.0.1:{port}").parse().expect("valid address");
    let task = tokio::spawn(async move {
        let _ = broker::serve(config, startup).await;
    });
    wait_for_healthz(address).await;
    RunningBroker { address, task }
}

/// Polls `GET /healthz` until the listener answers 200.
async fn wait_for_healthz(address: SocketAddr) {
    let deadline = Instant::now() + StdDuration::from_secs(10);
    loop {
        if let Ok(mut stream) = TcpStream::connect(address).await {
            let _ = stream
                .write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
                .await;
            let mut raw = Vec::new();
            let _ = stream.read_to_end(&mut raw).await;
            if raw.starts_with(b"HTTP/1.1 200") {
                return;
            }
        }
        if Instant::now() > deadline {
            panic!("broker under test did not become healthy in time");
        }
        tokio::time::sleep(StdDuration::from_millis(20)).await;
    }
}

/// Sends one HTTP/1.1 request by hand over a fresh `TcpStream` and returns the parsed
/// status code and JSON body. Always sends `Connection: close`, so reading to EOF is a
/// correct completeness signal.
async fn send(
    address: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: Option<&Value>,
) -> (u16, Value) {
    let mut stream = TcpStream::connect(address)
        .await
        .unwrap_or_else(|error| panic!("connect to {address} failed: {error}"));
    let payload = body.map(|value| serde_json::to_vec(value).expect("body serializes"));
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(payload) = &payload {
        request.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            payload.len()
        ));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request head");
    if let Some(payload) = &payload {
        stream.write_all(payload).await.expect("write request body");
    }
    stream.flush().await.expect("flush request");

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .await
        .expect("read response to EOF");
    let text = String::from_utf8_lossy(&raw);
    let mut sections = text.splitn(2, "\r\n\r\n");
    let head = sections.next().unwrap_or_default();
    let body_text = sections.next().unwrap_or_default();
    let status_line = head.lines().next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("unparseable status line {status_line:?} in {head:?}"));
    let value = if body_text.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(body_text)
            .unwrap_or_else(|error| panic!("response body is not valid JSON: {error}: {body_text}"))
    };
    (status, value)
}

/// The standard authenticated-context headers every protected `/v1/*` route (other than
/// `/v1/mcp`, which does its own bearer-only auth) requires.
fn context_headers(bearer: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Authorization", format!("Bearer {bearer}")),
        ("x-interface-version", CURRENT_INTERFACE_VERSION.to_owned()),
        ("x-correlation-id", Uuid::new_v4().to_string()),
        (
            "x-deadline",
            (Utc::now() + Duration::minutes(2)).to_rfc3339_opts(SecondsFormat::Millis, true),
        ),
    ]
}

/// Issues a bearer for `principal` with `capabilities` via `POST /v1/principals`,
/// authorized by `admin_bearer`. Panics with the response body on failure.
async fn issue_principal(
    address: SocketAddr,
    admin_bearer: &str,
    principal: Uuid,
    capabilities: &[&str],
) -> String {
    let mut headers = context_headers(admin_bearer);
    headers.push(("idempotency-key", format!("issue-{principal}")));
    let body = json!({
        "principalId": principal,
        "capabilities": capabilities,
        "expiresAt": (Utc::now() + Duration::minutes(10)).to_rfc3339_opts(SecondsFormat::Millis, true),
    });
    let (status, response) = send(address, "POST", "/v1/principals", &headers, Some(&body)).await;
    assert_eq!(status, 201, "principal issuance failed: {response}");
    response["bearer"]
        .as_str()
        .expect("issuance response carries a bearer")
        .to_owned()
}

/// Revokes `principal` via `DELETE /v1/principals/{id}`, authorized by `admin_bearer`.
async fn revoke_principal(address: SocketAddr, admin_bearer: &str, principal: Uuid) {
    let headers = context_headers(admin_bearer);
    let (status, response) = send(
        address,
        "DELETE",
        &format!("/v1/principals/{principal}"),
        &headers,
        None,
    )
    .await;
    assert_eq!(status, 204, "principal revocation failed: {response}");
}

/// Sends one JSON-RPC message to `POST /v1/mcp` with bearer-only auth, per
/// `broker::mcp_http::post_mcp`.
async fn post_mcp(address: SocketAddr, bearer: &str, message: Value) -> (u16, Value) {
    let headers = [("Authorization", format!("Bearer {bearer}"))];
    send(address, "POST", "/v1/mcp", &headers, Some(&message)).await
}

fn initialize_request(id: i64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "fleet-multi-principal-acceptance", "version": "1"}
        }
    })
}

fn initialized_notification() -> Value {
    json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})
}

fn tool_call_request(id: i64, name: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": name, "arguments": arguments}
    })
}

/// Runs one principal's full MCP lifecycle: `initialize`, `notifications/initialized`
/// (202 with no response object), then `tools/call runtime_info`. Returns the tool result.
async fn mcp_lifecycle_runtime_info(address: SocketAddr, bearer: &str) -> Value {
    let (status, body) = post_mcp(address, bearer, initialize_request(1)).await;
    assert_eq!(status, 200, "initialize failed: {body}");
    assert_eq!(
        body["result"]["protocolVersion"], "2025-11-25",
        "unexpected protocol version: {body}"
    );

    let (status, _body) = post_mcp(address, bearer, initialized_notification()).await;
    assert_eq!(
        status, 202,
        "notifications/initialized must be accepted with no body"
    );

    let (status, body) = post_mcp(
        address,
        bearer,
        tool_call_request(2, "runtime_info", json!({})),
    )
    .await;
    assert_eq!(status, 200, "runtime_info call failed: {body}");
    assert!(
        body.get("error").is_none(),
        "runtime_info must succeed for a principal holding session:read: {body}"
    );
    body["result"]["structuredContent"].clone()
}

/// Two independently capable principals, issued at runtime and driven over real HTTP
/// against `broker::serve`, including a restart against the same persisted authority store.
#[tokio::test]
async fn multi_principal_mcp_over_http_acceptance() {
    let root = tempfile::tempdir().expect("create acceptance root");
    let authority_path = root.path().join("authority.json");

    // "research" and "career-ops" team-driver principals.
    let principal_a = Uuid::new_v4();
    let principal_b = Uuid::new_v4();

    // --- First boot: issuance, concurrent independent MCP lifecycles, a
    // capability-gated denial that does not affect the unrelated principal, and
    // revocation taking effect immediately.
    let broker_one = boot_production(&authority_path, &root.path().join("boot-1"), 4, 2).await;

    let bearer_a = issue_principal(
        broker_one.address,
        BOOTSTRAP_BEARER,
        principal_a,
        &[
            "session:read",
            "session:write",
            "page:read",
            "page:write",
            "browser:mutate",
        ],
    )
    .await;
    let bearer_b = issue_principal(
        broker_one.address,
        BOOTSTRAP_BEARER,
        principal_b,
        &[
            "session:read",
            "session:write",
            "page:read",
            "page:write",
            "browser:mutate",
            "file:upload",
        ],
    )
    .await;

    // Each principal gets its own cached `mcp_gateway::Server`
    // (`broker::mcp_http::McpServers`), so neither lifecycle state machine observes the
    // other's `initialize`.
    let (result_a, result_b) = tokio::join!(
        mcp_lifecycle_runtime_info(broker_one.address, &bearer_a),
        mcp_lifecycle_runtime_info(broker_one.address, &bearer_b),
    );
    assert!(result_a.is_object(), "A's runtime_info result: {result_a}");
    assert!(result_b.is_object(), "B's runtime_info result: {result_b}");

    // B was never granted `recovery:write`, which `workflow_recover` requires per
    // `types::InterfaceOperation::RecoverWorkflow`. `Server::tool_available`/`call_tool`
    // decide this from the capability set alone, before any dispatch, so the call fails
    // closed as JSON-RPC "method not found" (-32601).
    let (status, denial) = post_mcp(
        broker_one.address,
        &bearer_b,
        tool_call_request(
            3,
            "workflow_recover",
            json!({"workflowId": Uuid::new_v4().to_string()}),
        ),
    )
    .await;
    assert_eq!(
        status, 200,
        "a JSON-RPC-level denial is still an HTTP 200: {denial}"
    );
    assert_eq!(
        denial["error"]["code"], -32601,
        "B lacks recovery:write, so workflow_recover must be reported as an unavailable \
         method rather than dispatched: {denial}"
    );

    // A is unaffected by B's denied call.
    let (status, body) = post_mcp(
        broker_one.address,
        &bearer_a,
        tool_call_request(4, "runtime_info", json!({})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.get("error").is_none(),
        "A must be unaffected by B's denial: {body}"
    );

    // Admin revokes B.
    revoke_principal(broker_one.address, BOOTSTRAP_BEARER, principal_b).await;

    // B's next `/v1/mcp` call is now unauthenticated.
    let (status, body) = post_mcp(broker_one.address, &bearer_b, initialize_request(5)).await;
    assert_eq!(status, 401, "revoked principal must be rejected: {body}");

    // A is still live.
    let (status, body) = post_mcp(
        broker_one.address,
        &bearer_a,
        tool_call_request(6, "runtime_info", json!({})),
    )
    .await;
    assert_eq!(status, 200, "revoking B must not affect A: {body}");
    assert!(body.get("error").is_none(), "{body}");

    broker_one.shutdown().await;

    // Restart: fresh process, same `authority_path` and bootstrap credential.
    // `PersistentAuthority` restores A's bearer and B's revocation from the same file.
    let broker_two = boot_production(&authority_path, &root.path().join("boot-2"), 4, 2).await;

    let restarted_result = mcp_lifecycle_runtime_info(broker_two.address, &bearer_a).await;
    assert!(
        restarted_result.is_object(),
        "A's bearer must survive the restart and get a fresh lifecycle: {restarted_result}"
    );

    let (status, body) = post_mcp(broker_two.address, &bearer_b, initialize_request(1)).await;
    assert_eq!(
        status, 401,
        "B must stay revoked across the restart, not resurrect from a stale in-memory \
         state: {body}"
    );

    broker_two.shutdown().await;
}
