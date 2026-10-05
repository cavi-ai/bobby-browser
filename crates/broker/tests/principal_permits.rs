//! Every exit path of a permit holder must return its permit.
//!
//! A gateway connection holds one runtime-wide connection permit
//! (`interface.max_connections`) and no per-principal permit; a plain or
//! streamable HTTP request holds a per-principal permit
//! (`interface.max_in_flight_per_principal`). Each cap is 1 where it is under
//! test, so a leaked permit makes the next connection or request refuse.

use std::net::SocketAddr;
use std::time::Duration;

use broker::{
    serve_listener,
    testing::{app_with_admin_and_limits, issue_bearer},
};
use futures_util::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue, Message};
use uuid::uuid;

const PRINCIPAL: uuid::Uuid = uuid!("00000000-0000-0000-0000-000000000031");
const ADMISSION_WINDOW: Duration = Duration::from_secs(5);

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

const DEFAULT_CONNECTIONS: usize = 64;

/// One principal permit, the default connection capacity: the HTTP rig.
async fn rig() -> Rig {
    rig_with(1, DEFAULT_CONNECTIONS).await
}

/// One connection slot, the default per-principal quota: the gateway rig. The
/// bearer is issued before the gateway opens, so issuance does not hold the slot.
async fn connection_rig(max_connections: usize) -> Rig {
    rig_with(8, max_connections).await
}

async fn rig_with(max_in_flight_per_principal: usize, max_connections: usize) -> Rig {
    let (app, _authority, admin) =
        app_with_admin_and_limits(4, max_in_flight_per_principal, max_connections).await;
    let bearer = issue_bearer(
        &app,
        &admin,
        PRINCIPAL,
        &["session:read", "session:write", "page:read", "page:write"],
    )
    .await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(serve_listener(listener, app, 64));
    Rig {
        address,
        bearer,
        server,
    }
}

async fn open_gateway(rig: &Rig) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
    let mut request = format!("ws://{}/v1/gateway/mcp", rig.address)
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", rig.bearer)).unwrap(),
    );
    tokio_tungstenite::connect_async(request)
        .await
        .map(|(socket, _)| socket)
}

/// A new gateway connection is admitted within the window, polling past the
/// time the runtime needs to notice the previous peer is gone.
async fn assert_admitted(rig: &Rig, what: &str) {
    let admitted = tokio::time::timeout(ADMISSION_WINDOW, async {
        loop {
            match open_gateway(rig).await {
                Ok(mut socket) => {
                    let _ = socket.close(None).await;
                    return;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
    })
    .await;
    assert!(
        admitted.is_ok(),
        "{what}: the connection permit was not released within {ADMISSION_WINDOW:?}"
    );
}

async fn assert_refused_while_held(rig: &Rig) {
    assert!(
        open_gateway(rig).await.is_err(),
        "max_connections of 1 must refuse a second gateway connection while the first is held"
    );
}

async fn http_status(rig: &Rig, request: &str) -> u16 {
    let mut stream = TcpStream::connect(rig.address).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut head = [0_u8; 12];
    stream.read_exact(&mut head).await.unwrap();
    std::str::from_utf8(&head[9..12]).unwrap().parse().unwrap()
}

fn long_poll(rig: &Rig) -> String {
    let deadline = (chrono::Utc::now() + chrono::Duration::minutes(2))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    format!(
        "GET /v1/events?after=0&limit=1 HTTP/1.1\r\nhost: localhost\r\nauthorization: Bearer {}\r\n\
         x-interface-version: {}\r\nx-correlation-id: 00000000-0000-4000-8000-000000000001\r\n\
         x-deadline: {deadline}\r\n\r\n",
        rig.bearer,
        types::CURRENT_INTERFACE_VERSION,
    )
}

fn mcp_initialize(rig: &Rig) -> String {
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#;
    format!(
        "POST /v1/mcp HTTP/1.1\r\nhost: localhost\r\nauthorization: Bearer {}\r\n\
         content-type: application/json\r\naccept: application/json, text/event-stream\r\n\
         content-length: {}\r\n\r\n{body}",
        rig.bearer,
        body.len()
    )
}

#[tokio::test]
async fn gateway_permit_returns_after_a_clean_close_while_idle() {
    let rig = connection_rig(1).await;
    let mut first = open_gateway(&rig).await.expect("first connection admitted");
    assert_refused_while_held(&rig).await;
    first.close(None).await.unwrap();
    while first.next().await.is_some() {}
    assert_admitted(&rig, "clean close").await;
}

#[tokio::test]
async fn gateway_permit_returns_after_an_abrupt_drop_while_idle() {
    let rig = connection_rig(1).await;
    let first = open_gateway(&rig).await.expect("first connection admitted");
    assert_refused_while_held(&rig).await;
    drop(first);
    assert_admitted(&rig, "abrupt drop").await;
}

#[tokio::test]
async fn gateway_permit_returns_after_a_drop_mid_request() {
    let rig = connection_rig(1).await;
    let mut first = open_gateway(&rig).await.expect("first connection admitted");
    first
        .send(Message::Text(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#.into(),
        ))
        .await
        .unwrap();
    drop(first);
    assert_admitted(&rig, "drop right after initialize").await;
}

#[tokio::test]
async fn gateway_connections_do_not_draw_on_the_principal_quota() {
    let rig = rig().await;
    let mut held = Vec::new();
    for attached in 1..=3 {
        held.push(
            open_gateway(&rig)
                .await
                .unwrap_or_else(|error| panic!("gateway {attached} refused: {error}")),
        );
    }
    assert_eq!(
        http_status(&rig, &mcp_initialize(&rig)).await,
        200,
        "an HTTP request is admitted while three gateways of the same principal are open"
    );
    drop(held);
}

#[tokio::test]
async fn gateway_refusal_at_connection_capacity_names_the_cap_and_a_retry_delay() {
    let rig = connection_rig(2).await;
    let _first = open_gateway(&rig).await.expect("first admitted");
    let _second = open_gateway(&rig).await.expect("second admitted");
    let refusal = match open_gateway(&rig).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => response,
        other => panic!("third gateway must be refused over HTTP, got {other:?}"),
    };
    let body = String::from_utf8(refusal.body().clone().unwrap_or_default()).unwrap();
    assert!(
        body.contains("gateway connection capacity exhausted"),
        "{body}"
    );
    assert!(
        body.contains("\"retryAfterMs\":1000") && refusal.headers().contains_key("retry-after"),
        "{body}"
    );
}

#[tokio::test]
async fn plain_http_permit_returns_when_the_client_leaves_a_running_handler() {
    let rig = rig().await;
    let mut parked = TcpStream::connect(rig.address).await.unwrap();
    parked.write_all(long_poll(&rig).as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        http_status(&rig, &long_poll(&rig)).await,
        429,
        "the parked long-poll must hold the only permit"
    );
    drop(parked);
    let freed = tokio::time::timeout(ADMISSION_WINDOW, async {
        loop {
            let mut probe = TcpStream::connect(rig.address).await.unwrap();
            probe.write_all(long_poll(&rig).as_bytes()).await.unwrap();
            let mut head = [0_u8; 12];
            let read =
                tokio::time::timeout(Duration::from_millis(300), probe.read_exact(&mut head)).await;
            // A probe that is admitted parks like the first one; a 429 answers at once.
            if read.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    assert!(
        freed.is_ok(),
        "plain HTTP permit was not released after the client disconnected"
    );
}

#[tokio::test]
async fn streamable_http_permit_returns_when_the_client_leaves_before_the_reply() {
    let rig = rig().await;
    let mut abandoned = TcpStream::connect(rig.address).await.unwrap();
    abandoned
        .write_all(mcp_initialize(&rig).as_bytes())
        .await
        .unwrap();
    drop(abandoned);
    assert_eq!(
        tokio::time::timeout(ADMISSION_WINDOW, async {
            loop {
                let status = http_status(&rig, &mcp_initialize(&rig)).await;
                if status != 429 {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("streamable HTTP permit was not released"),
        200
    );
}
