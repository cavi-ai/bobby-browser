//! Production-path broker bootstrap and a hand-written HTTP/1.1 client, shared by the
//! HTTP acceptance test and the HTTP performance arm. This workspace has no HTTP client
//! dependency, so requests are written over a loopback `TcpStream`.

use std::{
    net::SocketAddr,
    path::Path,
    time::{Duration as StdDuration, Instant},
};

use broker::StartupCredential;
use config::AppConfig;
use jiff::SignedDuration as Duration;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use types::{Capability, CURRENT_INTERFACE_VERSION};
use uuid::Uuid;

pub const BOOTSTRAP_BEARER: &str = "acceptance-bootstrap-bearer-0123456789abcdef01";

/// Grabs a free loopback port by binding then immediately dropping a listener. `serve`
/// takes a config-driven `host:port` and never reports the bound address, so the port has
/// to be known before the call.
pub async fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    listener
        .local_addr()
        .expect("listener has local addr")
        .port()
}

/// Config for the production `broker::serve` path: every mutable directory lives under
/// `data_root`, and the listener binds loopback `port`.
pub fn production_config(
    port: u16,
    authority_path: &Path,
    data_root: &Path,
    max_principals: usize,
    max_in_flight_per_principal: usize,
) -> AppConfig {
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
    config
}

/// The validated bootstrap credential the broker enrolls at startup.
pub fn startup_credential(capabilities: Vec<Capability>) -> StartupCredential {
    StartupCredential::new(
        BOOTSTRAP_BEARER.to_owned(),
        types::PrincipalId::from_uuid(Uuid::nil()),
        capabilities,
        types::WireTimestamp::now() + Duration::from_mins(30),
    )
    .expect("bootstrap startup credential is valid")
}

/// Polls `GET /healthz` until the listener answers 200.
pub async fn wait_for_healthz(address: SocketAddr) {
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
pub async fn send(
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
pub fn context_headers(bearer: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Authorization", format!("Bearer {bearer}")),
        ("x-interface-version", CURRENT_INTERFACE_VERSION.to_owned()),
        ("x-correlation-id", Uuid::new_v4().to_string()),
        (
            "x-deadline",
            (types::WireTimestamp::now() + Duration::from_mins(2)).rfc3339_millis(),
        ),
    ]
}

/// Issues a bearer for `principal` with `capabilities` via `POST /v1/principals`,
/// authorized by `admin_bearer`. Panics with the response body on failure.
pub async fn issue_principal(
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
        "expiresAt": (types::WireTimestamp::now() + Duration::from_mins(10)).rfc3339_millis(),
    });
    let (status, response) = send(address, "POST", "/v1/principals", &headers, Some(&body)).await;
    assert_eq!(status, 201, "principal issuance failed: {response}");
    response["bearer"]
        .as_str()
        .expect("issuance response carries a bearer")
        .to_owned()
}

/// Sends one JSON-RPC message to `POST /v1/mcp` with bearer-only auth, per
/// `broker::mcp_http::post_mcp`.
pub async fn post_mcp(address: SocketAddr, bearer: &str, message: Value) -> (u16, Value) {
    let headers = [("Authorization", format!("Bearer {bearer}"))];
    send(address, "POST", "/v1/mcp", &headers, Some(&message)).await
}
