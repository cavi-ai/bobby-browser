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

    let mut rejected = TcpStream::connect(address).await.unwrap();
    request(&mut rejected).await;
    let rejected_response = response_headers(&mut rejected).await;
    assert!(rejected_response.starts_with(b"HTTP/1.1 429"));
    assert!(rejected_response
        .windows(b"retry-after: 1".len())
        .any(|window| window.eq_ignore_ascii_case(b"retry-after: 1")));

    drop(first);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut retried = TcpStream::connect(address).await.unwrap();
    request(&mut retried).await;
    let admitted_response = response_headers(&mut retried).await;
    assert!(admitted_response.starts_with(b"HTTP/1.1 200"));

    drop(second);
    server.abort();
}
