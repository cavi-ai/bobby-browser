use std::time::Duration;

use firefox_companion::bidi::BidiClient;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{accept_async, tungstenite::Message, WebSocketStream};
use types::{ErrorCode, ErrorLayer};
use url::Url;

type ServerSocket = WebSocketStream<TcpStream>;

async fn server_socket(listener: TcpListener) -> ServerSocket {
    let (stream, _) = listener.accept().await.expect("accept connection");
    accept_async(stream).await.expect("accept websocket")
}

async fn recv_json(socket: &mut ServerSocket) -> Value {
    let message = socket
        .next()
        .await
        .expect("websocket message")
        .expect("valid websocket message");
    serde_json::from_str(message.to_text().expect("text message")).expect("json message")
}

async fn send_json(socket: &mut ServerSocket, value: Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .expect("send websocket message");
}

async fn listener_url() -> (TcpListener, Url) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind scripted server");
    let url = Url::parse(&format!("ws://{}", listener.local_addr().unwrap())).unwrap();
    (listener, url)
}

#[tokio::test]
async fn request_ids_are_monotonic_and_out_of_order_responses_stay_correlated() {
    let (listener, url) = listener_url().await;
    let server = tokio::spawn(async move {
        let mut socket = server_socket(listener).await;
        let first = recv_json(&mut socket).await;
        let second = recv_json(&mut socket).await;
        let first_id = first["id"].as_u64().unwrap();
        let second_id = second["id"].as_u64().unwrap();
        assert!(second_id > first_id);
        assert_eq!(first["method"], "first.command");
        assert_eq!(second["method"], "second.command");

        send_json(
            &mut socket,
            json!({"id": second_id, "type": "success", "result": {"name": "second"}}),
        )
        .await;
        send_json(
            &mut socket,
            json!({"id": first_id, "type": "success", "result": {"name": "first"}}),
        )
        .await;
    });

    let client = BidiClient::connect(url, Duration::from_secs(1))
        .await
        .unwrap();
    let first_client = client.clone();
    let first = tokio::spawn(async move {
        first_client
            .send("first.command", json!({"position": 1}))
            .await
    });
    tokio::task::yield_now().await;
    let second =
        tokio::spawn(async move { client.send("second.command", json!({"position": 2})).await });

    assert_eq!(first.await.unwrap().unwrap(), json!({"name": "first"}));
    assert_eq!(second.await.unwrap().unwrap(), json!({"name": "second"}));
    server.await.unwrap();
}

#[tokio::test]
async fn events_are_delivered_independently_of_command_responses() {
    let (listener, url) = listener_url().await;
    let server = tokio::spawn(async move {
        let mut socket = server_socket(listener).await;
        let command = recv_json(&mut socket).await;
        send_json(
            &mut socket,
            json!({"method": "log.entryAdded", "params": {"text": "ready"}}),
        )
        .await;
        send_json(
            &mut socket,
            json!({"id": command["id"], "type": "success", "result": {"ok": true}}),
        )
        .await;
    });

    let client = BidiClient::connect(url, Duration::from_secs(1))
        .await
        .unwrap();
    let mut events = client.subscribe_events();
    let response = client.send("session.status", json!({})).await.unwrap();
    let event = events.recv().await.unwrap();

    assert_eq!(response, json!({"ok": true}));
    assert_eq!(event.method, "log.entryAdded");
    assert_eq!(event.params, json!({"text": "ready"}));
    server.await.unwrap();
}

#[tokio::test]
async fn deadline_removes_pending_correlation_without_poisoning_the_client() {
    let (listener, url) = listener_url().await;
    let server = tokio::spawn(async move {
        let mut socket = server_socket(listener).await;
        let first = recv_json(&mut socket).await;
        assert_eq!(first["method"], "never.respond");
        let second = recv_json(&mut socket).await;
        send_json(
            &mut socket,
            json!({"id": second["id"], "type": "success", "result": {"alive": true}}),
        )
        .await;
    });

    let client = BidiClient::connect(url, Duration::from_millis(100))
        .await
        .unwrap();
    let error = client.send("never.respond", json!({})).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::DeadlineExceeded);
    assert_eq!(error.layer, ErrorLayer::Driver);
    assert!(error.retryable);

    assert_eq!(
        client.send("still.alive", json!({})).await.unwrap(),
        json!({"alive": true})
    );
    server.await.unwrap();
}

#[tokio::test]
async fn unknown_response_id_rejects_the_pending_command() {
    let (listener, url) = listener_url().await;
    let server = tokio::spawn(async move {
        let mut socket = server_socket(listener).await;
        let command = recv_json(&mut socket).await;
        let unknown = command["id"].as_u64().unwrap() + 100;
        send_json(
            &mut socket,
            json!({"id": unknown, "type": "success", "result": {}}),
        )
        .await;
    });

    let client = BidiClient::connect(url, Duration::from_secs(1))
        .await
        .unwrap();
    let error = client.send("session.status", json!({})).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::BrowserCommandFailed);
    assert_eq!(error.layer, ErrorLayer::Driver);
    assert!(!error.retryable);
    server.await.unwrap();
}

#[tokio::test]
async fn disconnect_fails_pending_commands_as_retryable_driver_errors() {
    let (listener, url) = listener_url().await;
    let server = tokio::spawn(async move {
        let mut socket = server_socket(listener).await;
        let _ = recv_json(&mut socket).await;
        socket.close(None).await.unwrap();
    });

    let client = BidiClient::connect(url, Duration::from_secs(1))
        .await
        .unwrap();
    let error = client.send("session.status", json!({})).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::BrowserCommandFailed);
    assert_eq!(error.layer, ErrorLayer::Driver);
    assert!(error.retryable);
    server.await.unwrap();
}
