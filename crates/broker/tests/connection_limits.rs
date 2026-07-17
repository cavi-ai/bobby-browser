use std::time::Duration;

use axum::{routing::get, Router};
use broker::serve_listener;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

async fn request(stream: &mut TcpStream) {
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
}

async fn response_headers(stream: &mut TcpStream) -> Vec<u8> {
    let mut response = Vec::new();
    let mut chunk = [0_u8; 256];
    while !response.windows(4).any(|window| window == b"\r\n\r\n") {
        let count = stream.read(&mut chunk).await.unwrap();
        assert!(count > 0, "server closed before returning response headers");
        response.extend_from_slice(&chunk[..count]);
    }
    response
}

#[tokio::test]
async fn listener_admits_only_the_configured_number_of_live_connections() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().route("/healthz", get(|| async { "ok" }));
    let server = tokio::spawn(serve_listener(listener, app, 2));

    let mut first = TcpStream::connect(address).await.unwrap();
    request(&mut first).await;
    let first_response = response_headers(&mut first).await;
    assert!(first_response.starts_with(b"HTTP/1.1 200"));

    let mut second = TcpStream::connect(address).await.unwrap();
    request(&mut second).await;
    let second_response = response_headers(&mut second).await;
    assert!(second_response.starts_with(b"HTTP/1.1 200"));

    let mut queued_in_kernel = TcpStream::connect(address).await.unwrap();
    request(&mut queued_in_kernel).await;
    let mut byte = [0_u8; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(150), queued_in_kernel.read(&mut byte))
            .await
            .is_err(),
        "the next connection was accepted while the first remained live"
    );

    drop(first);
    let admitted_response = tokio::time::timeout(
        Duration::from_secs(2),
        response_headers(&mut queued_in_kernel),
    )
    .await
    .expect("queued connection should proceed after a permit is released");
    assert!(admitted_response.starts_with(b"HTTP/1.1 200"));

    drop(second);
    server.abort();
}
