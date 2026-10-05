//! A gateway connection that ends, cleanly or abruptly, closes the sessions
//! it opened: the principal's session list empties after the peer leaves.

use std::{net::SocketAddr, time::Duration};

use broker::{
    serve_listener,
    testing::{app_with_admin_and_quota, issue_bearer},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue, Message};
use uuid::uuid;

const PRINCIPAL: uuid::Uuid = uuid!("00000000-0000-0000-0000-000000000041");
const CLEANUP_WINDOW: Duration = Duration::from_secs(10);

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

struct Rig {
    address: SocketAddr,
    bearer: String,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn rig() -> Rig {
    let (app, _authority, admin) = app_with_admin_and_quota(4, 4).await;
    let bearer = issue_bearer(&app, &admin, PRINCIPAL, &["session:read", "session:write"]).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(serve_listener(listener, app, 64));
    Rig {
        address,
        bearer,
        server,
    }
}

async fn open_gateway(rig: &Rig) -> Socket {
    let mut request = format!("ws://{}/v1/gateway/mcp", rig.address)
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", rig.bearer)).unwrap(),
    );
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

async fn send(socket: &mut Socket, message: Value) {
    socket
        .send(Message::Text(message.to_string().into()))
        .await
        .unwrap();
}

async fn response(socket: &mut Socket, id: u64) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let Message::Text(text) = socket.next().await.expect("gateway open").unwrap() else {
                continue;
            };
            let value: Value = serde_json::from_str(text.as_str()).unwrap();
            if value["id"] == json!(id) {
                return value;
            }
        }
    })
    .await
    .expect("response arrives")
}

/// Opens a gateway connection and has it create one session.
async fn connection_with_a_session(rig: &Rig) -> Socket {
    let mut socket = open_gateway(rig).await;
    send(
        &mut socket,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":{},
            "clientInfo":{"name":"t","version":"0"}}}),
    )
    .await;
    assert!(response(&mut socket, 1).await.get("result").is_some());
    send(
        &mut socket,
        json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
    )
    .await;
    send(
        &mut socket,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"session_create","arguments":{"profile":"default"}}}),
    )
    .await;
    let created = response(&mut socket, 2).await;
    assert!(
        created["result"]["structuredContent"]["id"].is_string(),
        "{created}"
    );
    socket
}

async fn listed_sessions(rig: &Rig) -> usize {
    let deadline = (chrono::Utc::now() + chrono::Duration::minutes(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let request = format!(
        "GET /v1/sessions HTTP/1.1\r\nhost: localhost\r\nauthorization: Bearer {}\r\n\
         x-interface-version: {}\r\nx-correlation-id: 00000000-0000-4000-8000-000000000002\r\n\
         x-deadline: {deadline}\r\nconnection: close\r\n\r\n",
        rig.bearer,
        types::CURRENT_INTERFACE_VERSION,
    );
    let mut stream = TcpStream::connect(rig.address).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").expect("http response");
    assert!(head.starts_with("HTTP/1.1 200"), "{text}");
    let body = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        body.lines().skip(1).step_by(2).collect::<Vec<_>>().join("")
    } else {
        body.to_owned()
    };
    let value: Value = serde_json::from_str(&body).unwrap_or_else(|_| panic!("{text}"));
    value
        .as_array()
        .or_else(|| value["sessions"].as_array())
        .unwrap_or_else(|| panic!("{value}"))
        .len()
}

async fn assert_sessions_close(rig: &Rig, what: &str) {
    let closed = tokio::time::timeout(CLEANUP_WINDOW, async {
        loop {
            if listed_sessions(rig).await == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        closed.is_ok(),
        "{what}: the session was still listed {CLEANUP_WINDOW:?} after the peer left"
    );
}

#[tokio::test]
async fn gateway_sessions_close_after_an_abrupt_drop() {
    let rig = rig().await;
    let socket = connection_with_a_session(&rig).await;
    assert_eq!(listed_sessions(&rig).await, 1);
    drop(socket);
    assert_sessions_close(&rig, "abrupt drop").await;
}

#[tokio::test]
async fn gateway_sessions_close_after_a_clean_close() {
    let rig = rig().await;
    let mut socket = connection_with_a_session(&rig).await;
    assert_eq!(listed_sessions(&rig).await, 1);
    socket.close(None).await.unwrap();
    while socket.next().await.is_some() {}
    assert_sessions_close(&rig, "clean close").await;
}
