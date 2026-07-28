use std::time::Duration;

use firefox_companion::bidi::BidiClient;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tokio_tungstenite::{accept_async, tungstenite::Message, WebSocketStream};
use types::{ErrorCode, ErrorLayer};
use url::Url;

type ServerSocket = WebSocketStream<TcpStream>;

async fn server_socket(listener: TcpListener) -> ServerSocket {
    let (stream, _) = listener.accept().await.expect("accept connection");
    accept_async(stream).await.expect("accept websocket")
}

async fn next_server_socket(listener: &TcpListener) -> ServerSocket {
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
async fn session_connection_negotiates_a_bidi_session_before_use() {
    let (listener, url) = listener_url().await;
    let server = tokio::spawn(async move {
        let mut socket = server_socket(listener).await;
        let command = recv_json(&mut socket).await;
        assert_eq!(command["method"], "session.new");
        assert_eq!(
            command["params"],
            json!({"capabilities": {"alwaysMatch": {}}})
        );
        send_json(
            &mut socket,
            json!({"id": command["id"], "type": "success", "result": {"sessionId": "session-1", "capabilities": {}}}),
        )
        .await;
    });

    BidiClient::connect_session(url, Duration::from_secs(1))
        .await
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn ending_session_releases_the_single_session_slot_before_handoff() {
    let (listener, url) = listener_url().await;
    let server = tokio::spawn(async move {
        let mut first = next_server_socket(&listener).await;
        let session_new = recv_json(&mut first).await;
        assert_eq!(session_new["method"], "session.new");
        send_json(
            &mut first,
            json!({"id": session_new["id"], "type": "success", "result": {"sessionId": "installer", "capabilities": {}}}),
        )
        .await;

        let session_end = recv_json(&mut first).await;
        assert_eq!(session_end["method"], "session.end");
        assert_eq!(session_end["params"], json!({}));
        send_json(
            &mut first,
            json!({"id": session_end["id"], "type": "success", "result": {}}),
        )
        .await;
        drop(first);

        let mut second = next_server_socket(&listener).await;
        let handoff = recv_json(&mut second).await;
        assert_eq!(handoff["method"], "session.new");
        send_json(
            &mut second,
            json!({"id": handoff["id"], "type": "success", "result": {"sessionId": "runtime", "capabilities": {}}}),
        )
        .await;
    });

    let installer = BidiClient::connect_session(url.clone(), Duration::from_secs(1))
        .await
        .unwrap();
    installer.end_session().await.unwrap();

    BidiClient::connect_session(url, Duration::from_secs(1))
        .await
        .unwrap();
    server.await.unwrap();
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
            json!({"type": "event", "method": "log.entryAdded", "params": {"text": "ready"}}),
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
async fn response_deadline_terminates_the_uncertain_session_before_replacement() {
    let (listener, url) = listener_url().await;
    let server = tokio::spawn(async move {
        let mut socket = server_socket(listener).await;
        let first = recv_json(&mut socket).await;
        assert_eq!(first["method"], "never.respond");
        let closed = tokio::time::timeout(Duration::from_secs(1), socket.next())
            .await
            .expect("transport closes promptly after a response deadline");
        assert!(matches!(closed, None | Some(Ok(Message::Close(_)))));
    });

    let client = BidiClient::connect(url, Duration::from_millis(100))
        .await
        .unwrap();
    let error = client.send("never.respond", json!({})).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::DeadlineExceeded);
    assert_eq!(error.layer, ErrorLayer::Driver);
    assert!(error.retryable);
    assert_eq!(
        error.message,
        "Firefox BiDi never.respond response deadline exceeded"
    );

    let follow_up = client.send("must.replace", json!({})).await.unwrap_err();
    assert_eq!(follow_up.code, ErrorCode::DeadlineExceeded);
    assert_eq!(follow_up.layer, ErrorLayer::Driver);
    assert!(follow_up.retryable);
    assert_eq!(follow_up.message, error.message);
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

async fn malformed_message_error(message: Value) -> types::CommandError {
    let (listener, url) = listener_url().await;
    let server = tokio::spawn(async move {
        let mut socket = server_socket(listener).await;
        let command = recv_json(&mut socket).await;
        let mut message = message;
        if message.get("id") == Some(&Value::String("request".into())) {
            message["id"] = command["id"].clone();
        }
        send_json(&mut socket, message).await;
    });
    let client = BidiClient::connect(url, Duration::from_secs(1))
        .await
        .unwrap();
    let error = client.send("session.status", json!({})).await.unwrap_err();
    server.await.unwrap();
    error
}

#[tokio::test]
async fn malformed_response_and_event_envelopes_fail_the_transport() {
    let malformed = [
        json!({"id": "request", "result": {}}),
        json!({"id": "request", "type": "event", "result": {}}),
        json!({"id": "request", "type": "success"}),
        json!({"id": "request", "type": "success", "result": null}),
        json!({"id": "request", "type": "success", "result": "not-a-map"}),
        json!({"id": "request", "type": "success", "result": []}),
        json!({"id": "not-a-number", "type": "success", "result": {}}),
        json!({"method": "log.entryAdded", "params": {}}),
        json!({"type": "event", "method": "log.entryAdded"}),
        json!({"type": "event", "method": 7, "params": {}}),
        json!({"type": "event", "method": "log.entryAdded", "params": null}),
        json!({"type": "event", "method": "log.entryAdded", "params": "not-a-map"}),
        json!({"type": "event", "method": "log.entryAdded", "params": []}),
    ];

    for message in malformed {
        let error = malformed_message_error(message).await;
        assert_eq!(error.code, ErrorCode::BrowserCommandFailed);
        assert_eq!(error.layer, ErrorLayer::Driver);
        assert!(!error.retryable);
    }
}

#[tokio::test]
async fn repeated_aborted_sends_release_capacity_and_consume_late_responses() {
    const ABORTED: usize = 257;
    let (listener, url) = listener_url().await;
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let mut socket = server_socket(listener).await;
        let mut withheld = Vec::with_capacity(ABORTED);
        for _ in 0..ABORTED {
            let command = recv_json(&mut socket).await;
            let id = command["id"].as_u64().unwrap();
            seen_tx.send(id).unwrap();
            withheld.push(id);
        }
        send_json(
            &mut socket,
            json!({"id": withheld[0], "type": "success", "result": {"late": true}}),
        )
        .await;
        let live = recv_json(&mut socket).await;
        send_json(
            &mut socket,
            json!({"id": live["id"], "type": "success", "result": {"alive": true}}),
        )
        .await;
    });

    let client = BidiClient::connect(url, Duration::from_secs(2))
        .await
        .unwrap();
    for index in 0..ABORTED {
        let sender = client.clone();
        let task = tokio::spawn(async move {
            sender
                .send("aborted.command", json!({"index": index}))
                .await
        });
        let _id = seen_rx.recv().await.unwrap();
        task.abort();
        let _ = task.await;
        tokio::task::yield_now().await;
    }

    assert_eq!(
        client.send("live.command", json!({})).await.unwrap(),
        json!({"alive": true})
    );
    server.await.unwrap();
}

#[tokio::test]
async fn close_preempts_queued_effect_commands_and_is_idempotent() {
    let (listener, url) = listener_url().await;
    let server = tokio::spawn(async move {
        let mut socket = server_socket(listener).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        let mut effects = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(2), socket.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(value) = serde_json::from_str::<Value>(text.as_str()) {
                        if value["method"]
                            .as_str()
                            .is_some_and(|method| method.starts_with("effect."))
                        {
                            effects.push(value);
                        }
                    }
                }
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Err(_) => break,
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(_))) => break,
            }
        }
        effects
    });

    let client = BidiClient::connect(url, Duration::from_secs(3))
        .await
        .unwrap();
    let blocker_client = client.clone();
    let blocker = tokio::spawn(async move {
        blocker_client
            .send(
                "blocker.command",
                json!({"padding": "x".repeat(8 * 1024 * 1024)}),
            )
            .await
    });
    tokio::task::yield_now().await;
    let queued = (0..8)
        .map(|index| {
            let sender = client.clone();
            tokio::spawn(async move { sender.send(&format!("effect.{index}"), json!({})).await })
        })
        .collect::<Vec<_>>();
    tokio::task::yield_now().await;

    let _ = client.close().await;
    let _ = client.close().await;
    blocker.abort();
    for task in queued {
        task.abort();
    }

    assert!(server.await.unwrap().is_empty());
}
