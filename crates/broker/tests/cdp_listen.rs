use std::net::SocketAddr;

use broker::testing;
use config::CdpConfig;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

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
    let (cdp, bearer) = testing::spawn_test_cdp_listener(CdpConfig {
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
