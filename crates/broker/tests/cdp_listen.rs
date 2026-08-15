use std::net::SocketAddr;

use broker::testing;
use config::CdpConfig;
use futures_util::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue, Message};

async fn http_get(addr: SocketAddr, path: &str, bearer: Option<&str>) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect to listener");
    let host = format!("{addr}");
    let auth = bearer
        .map(|token| format!("authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let request = format!("GET {path} HTTP/1.1\r\nhost: {host}\r\n{auth}connection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    let mut body = String::new();
    stream
        .read_to_string(&mut body)
        .await
        .expect("read response");
    let status = body
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, body)
}

#[tokio::test]
async fn cdp_listener_serves_json_version_when_enabled() {
    let (cdp, _authority, bearer) = testing::spawn_test_cdp_listener(CdpConfig {
        enabled: true,
        host: "127.0.0.1".into(),
        port: 0,
    })
    .await;
    let (status, body) = http_get(cdp.addr, "/json/version", Some(&bearer)).await;
    assert_eq!(status, 200, "response: {body}");
    assert!(
        body.contains("webSocketDebuggerUrl"),
        "expected discovery payload, got: {body}"
    );
    cdp.handle.abort();
}

#[tokio::test]
async fn cdp_bind_failure_names_the_address_and_the_port_override() {
    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("occupy a port");
    let port = occupied.local_addr().expect("bound address").port();

    let (listen, _authority, _bearer) = testing::try_spawn_test_cdp_listener(CdpConfig {
        enabled: true,
        host: "127.0.0.1".into(),
        port,
    })
    .await;

    let error = format!(
        "{:#}",
        listen.err().expect("bind fails on an occupied port")
    );
    assert!(error.contains(&format!("127.0.0.1:{port}")), "{error}");
    assert!(error.contains("--cdp-port"), "{error}");
}

#[tokio::test]
async fn cdp_listener_binds_runtime_for_an_issued_principal() {
    let (cdp, authority, _startup_bearer) = testing::spawn_test_cdp_listener(CdpConfig {
        enabled: true,
        host: "127.0.0.1".into(),
        port: 0,
    })
    .await;
    let principal = types::PrincipalId::from_uuid(uuid::Uuid::new_v4());
    let bearer = authority
        .issue(
            principal,
            vec![types::Capability::SessionRead],
            chrono::Utc::now() + chrono::Duration::minutes(10),
        )
        .await
        .expect("principal bearer issues")
        .expose_once();

    let (status, body) = http_get(cdp.addr, "/json/list", Some(&bearer)).await;
    assert_eq!(status, 200, "response: {body}");

    let (status, body) = http_get(cdp.addr, "/json/version", Some(&bearer)).await;
    assert_eq!(status, 200, "response: {body}");
    let payload = body.split("\r\n\r\n").nth(1).expect("response has body");
    let websocket_url = serde_json::from_str::<serde_json::Value>(payload)
        .expect("version response is JSON")["webSocketDebuggerUrl"]
        .as_str()
        .expect("version response carries websocket URL")
        .to_owned();
    let mut request = websocket_url.into_client_request().expect("request builds");
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {bearer}")).expect("bearer header is valid"),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("issued principal upgrades websocket");
    socket
        .send(Message::Text(
            r#"{"id":1,"method":"Target.getTargets","params":{}}"#.into(),
        ))
        .await
        .expect("session command sends");
    let response = socket
        .next()
        .await
        .expect("session command responds")
        .expect("websocket response is valid")
        .into_text()
        .expect("response is text");
    let response: serde_json::Value = serde_json::from_str(&response).expect("response is JSON");
    assert_eq!(response["id"], 1);
    assert!(response.get("error").is_none(), "response: {response}");
    socket.close(None).await.expect("websocket closes");
    cdp.handle.abort();
}

#[tokio::test]
async fn http_port_does_not_expose_json_version_when_cdp_disabled() {
    let (app, _authority, bearer) = testing::app_with_admin(4).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind http listener");
    let addr = listener.local_addr().expect("local addr");
    let server = tokio::spawn(broker::serve_listener(listener, app, 8));
    let (status, _body) = http_get(addr, "/json/version", Some(&bearer)).await;
    server.abort();
    assert_eq!(
        status, 404,
        "HTTP router must not mount CDP discovery routes"
    );
}
