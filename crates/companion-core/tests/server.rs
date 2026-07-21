use companion_core::{CompanionServer, CompanionServerConfig, CompanionServerError, PairingInput};
use companion_protocol::{CompanionEvent, CompanionRequest, PairRequest};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::{net::SocketAddr, time::Duration};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::StatusCode, Error, Message},
    MaybeTlsStream, WebSocketStream,
};

const PRIVATE_BEARER: &str = "private-bearer-that-must-never-appear";
const PRIVATE_FRAME_MARKER: &str = "private-frame-body-that-must-never-appear";

type ClientSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

fn test_config(bind_addr: SocketAddr) -> CompanionServerConfig {
    CompanionServerConfig {
        bind_addr,
        pairing_code_ttl: Duration::from_secs(60),
        attachment_ttl: Duration::from_secs(300),
    }
}

fn loopback_config() -> CompanionServerConfig {
    test_config("127.0.0.1:0".parse().unwrap())
}

fn endpoint(addr: SocketAddr) -> String {
    format!("ws://{addr}/v1/companion")
}

async fn connect_without_bearer(addr: SocketAddr) -> Result<ClientSocket, Error> {
    connect_async(endpoint(addr))
        .await
        .map(|(socket, _)| socket)
}

async fn connect_with_bearer(addr: SocketAddr, bearer: &str) -> Result<ClientSocket, Error> {
    let mut request = endpoint(addr).into_client_request().unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {bearer}").parse().unwrap());
    connect_async(request).await.map(|(socket, _)| socket)
}

fn http_status(error: Error) -> StatusCode {
    match error {
        Error::Http(response) => response.status(),
        other => panic!("expected HTTP handshake rejection, got {other:?}"),
    }
}

fn http_body(error: Error) -> String {
    match error {
        Error::Http(response) => response
            .body()
            .as_deref()
            .map(String::from_utf8_lossy)
            .map(|body| body.into_owned())
            .unwrap_or_default(),
        other => panic!("expected HTTP handshake rejection, got {other:?}"),
    }
}

fn pair_request(code: String) -> CompanionRequest {
    let input = PairingInput::firefox(code.clone());
    CompanionRequest::Pair(PairRequest {
        protocol_version: companion_protocol::PROTOCOL_VERSION,
        pairing_code: code,
        companion_id: input.companion_id,
        profile_id: input.profile_id,
        identity: input.identity,
        capabilities: input.capabilities,
    })
}

async fn send_request(socket: &mut ClientSocket, request: &CompanionRequest) {
    socket
        .send(Message::Text(
            serde_json::to_string(request).unwrap().into(),
        ))
        .await
        .unwrap();
}

async fn receive_event(socket: &mut ClientSocket) -> CompanionEvent {
    loop {
        match socket.next().await.unwrap().unwrap() {
            Message::Text(text) => {
                let mut value: Value = serde_json::from_str(text.as_str()).unwrap();
                if value["kind"] == "paired" {
                    value["output"]
                        .as_object_mut()
                        .unwrap()
                        .remove("reconnectCredential");
                }
                return serde_json::from_value(value).unwrap();
            }
            Message::Ping(payload) => socket.send(Message::Pong(payload)).await.unwrap(),
            other => panic!("expected companion event, got {other:?}"),
        }
    }
}

async fn pair_with_credential(socket: &mut ClientSocket, code: String) -> (CompanionEvent, String) {
    send_request(socket, &pair_request(code)).await;
    loop {
        match socket.next().await.unwrap().unwrap() {
            Message::Text(text) => {
                let mut value: Value = serde_json::from_str(text.as_str()).unwrap();
                let credential = value["output"]["reconnectCredential"]
                    .as_str()
                    .expect("paired wire event must include reconnect credential")
                    .to_owned();
                value["output"]
                    .as_object_mut()
                    .unwrap()
                    .remove("reconnectCredential");
                return (serde_json::from_value(value).unwrap(), credential);
            }
            Message::Ping(payload) => socket.send(Message::Pong(payload)).await.unwrap(),
            other => panic!("expected paired wire event, got {other:?}"),
        }
    }
}

async fn pair(socket: &mut ClientSocket, code: String) -> CompanionEvent {
    send_request(socket, &pair_request(code)).await;
    receive_event(socket).await
}

#[tokio::test]
async fn non_loopback_bind_address_is_rejected() {
    let error = CompanionServer::bind_loopback(test_config("0.0.0.0:0".parse().unwrap()))
        .await
        .unwrap_err();

    assert!(matches!(error, CompanionServerError::NonLoopbackAddress(_)));
}

#[tokio::test]
async fn missing_bearer_is_rejected() {
    let server = CompanionServer::bind_loopback(loopback_config())
        .await
        .unwrap();

    let error = connect_without_bearer(server.local_addr())
        .await
        .unwrap_err();

    assert_eq!(http_status(error), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn incorrect_bearer_is_rejected() {
    let server = CompanionServer::bind_loopback(loopback_config())
        .await
        .unwrap();

    let error = connect_with_bearer(server.local_addr(), PRIVATE_BEARER)
        .await
        .unwrap_err();

    assert_eq!(http_status(error), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn pairing_bearer_is_single_use() {
    let server = CompanionServer::bind_loopback(loopback_config())
        .await
        .unwrap();
    let code = server.registry().issue_pairing_code().await;
    let mut socket = connect_with_bearer(server.local_addr(), &code)
        .await
        .unwrap();

    assert!(matches!(
        pair(&mut socket, code.clone()).await,
        CompanionEvent::Paired { .. }
    ));

    let error = connect_with_bearer(server.local_addr(), &code)
        .await
        .unwrap_err();
    assert_eq!(http_status(error), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn reconnect_credential_resumes_without_reusing_the_pairing_code() {
    let server = CompanionServer::bind_loopback(loopback_config())
        .await
        .unwrap();
    let code = server.registry().issue_pairing_code().await;
    let mut first = connect_with_bearer(server.local_addr(), &code)
        .await
        .unwrap();
    let (paired, credential) = pair_with_credential(&mut first, code.clone()).await;
    assert!(matches!(paired, CompanionEvent::Paired { .. }));
    first.close(None).await.unwrap();

    let mut resumed = connect_with_bearer(server.local_addr(), &credential)
        .await
        .unwrap();
    assert!(matches!(
        receive_event(&mut resumed).await,
        CompanionEvent::Paired { .. }
    ));
    send_request(&mut resumed, &CompanionRequest::Ping).await;
    assert_eq!(receive_event(&mut resumed).await, CompanionEvent::Pong);

    let error = connect_with_bearer(server.local_addr(), &code)
        .await
        .unwrap_err();
    assert_eq!(http_status(error), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn revocation_invalidates_the_reconnect_credential() {
    let server = CompanionServer::bind_loopback(loopback_config())
        .await
        .unwrap();
    let code = server.registry().issue_pairing_code().await;
    let mut socket = connect_with_bearer(server.local_addr(), &code)
        .await
        .unwrap();
    let (paired, credential) = pair_with_credential(&mut socket, code).await;
    let CompanionEvent::Paired { companion_id, .. } = paired else {
        panic!("expected paired event");
    };
    socket.close(None).await.unwrap();
    server.registry().revoke(&companion_id).await.unwrap();

    let error = connect_with_bearer(server.local_addr(), &credential)
        .await
        .unwrap_err();
    assert_eq!(http_status(error), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn pairing_bearer_is_claimed_by_first_handshake() {
    let server = CompanionServer::bind_loopback(loopback_config())
        .await
        .unwrap();
    let code = server.registry().issue_pairing_code().await;
    let mut first = connect_with_bearer(server.local_addr(), &code)
        .await
        .unwrap();

    let error = connect_with_bearer(server.local_addr(), &code)
        .await
        .unwrap_err();

    assert_eq!(http_status(error), StatusCode::UNAUTHORIZED);
    assert!(matches!(
        pair(&mut first, code).await,
        CompanionEvent::Paired { .. }
    ));
}

#[tokio::test]
async fn disconnect_before_pairing_invalidates_claimed_bearer() {
    let server = CompanionServer::bind_loopback(loopback_config())
        .await
        .unwrap();
    let code = server.registry().issue_pairing_code().await;
    let mut socket = connect_with_bearer(server.local_addr(), &code)
        .await
        .unwrap();
    socket.close(None).await.unwrap();

    let error = connect_with_bearer(server.local_addr(), &code)
        .await
        .unwrap_err();

    assert_eq!(http_status(error), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn ping_after_pairing_receives_pong() {
    let server = CompanionServer::bind_loopback(loopback_config())
        .await
        .unwrap();
    let code = server.registry().issue_pairing_code().await;
    let mut socket = connect_with_bearer(server.local_addr(), &code)
        .await
        .unwrap();
    pair(&mut socket, code).await;

    send_request(&mut socket, &CompanionRequest::Ping).await;

    assert_eq!(receive_event(&mut socket).await, CompanionEvent::Pong);
}

#[tokio::test]
async fn first_request_must_pair() {
    let server = CompanionServer::bind_loopback(loopback_config())
        .await
        .unwrap();
    let code = server.registry().issue_pairing_code().await;
    let mut socket = connect_with_bearer(server.local_addr(), &code)
        .await
        .unwrap();

    send_request(&mut socket, &CompanionRequest::Ping).await;

    let Message::Text(body) = socket.next().await.unwrap().unwrap() else {
        panic!("expected typed transport error");
    };
    let error: Value = serde_json::from_str(body.as_str()).unwrap();
    assert_eq!(error["code"], "pairingRequired");
    assert_eq!(error["message"], "the first request must pair");
}

#[tokio::test]
async fn duplicate_object_keys_are_rejected() {
    let server = CompanionServer::bind_loopback(loopback_config())
        .await
        .unwrap();
    let code = server.registry().issue_pairing_code().await;
    let mut socket = connect_with_bearer(server.local_addr(), &code)
        .await
        .unwrap();

    socket
        .send(Message::Text(r#"{"kind":"pair","kind":"ping"}"#.into()))
        .await
        .unwrap();

    let Message::Text(body) = socket.next().await.unwrap().unwrap() else {
        panic!("expected typed transport error");
    };
    let error: Value = serde_json::from_str(body.as_str()).unwrap();
    assert_eq!(error["code"], "invalidRequest");
    assert_eq!(error["message"], "request must be strict companion JSON");
}

#[tokio::test]
async fn oversized_frame_closes_with_typed_secret_free_error() {
    let server = CompanionServer::bind_loopback(loopback_config())
        .await
        .unwrap();
    let code = server.registry().issue_pairing_code().await;
    let mut socket = connect_with_bearer(server.local_addr(), &code)
        .await
        .unwrap();
    pair(&mut socket, code).await;
    let oversized = format!("{PRIVATE_FRAME_MARKER}{}", "x".repeat(1024 * 1024));

    socket.send(Message::Text(oversized.into())).await.unwrap();

    let Message::Text(body) = socket.next().await.unwrap().unwrap() else {
        panic!("expected typed transport error");
    };
    assert!(!body.contains(PRIVATE_FRAME_MARKER));
    let error: Value = serde_json::from_str(body.as_str()).unwrap();
    assert_eq!(error["code"], "frameTooLarge");
    assert_eq!(error["message"], "frame exceeds the 1 MiB limit");
    assert!(matches!(
        socket.next().await.unwrap().unwrap(),
        Message::Close(_)
    ));
}

#[tokio::test]
async fn authentication_error_does_not_echo_bearer() {
    let server = CompanionServer::bind_loopback(loopback_config())
        .await
        .unwrap();

    let error = connect_with_bearer(server.local_addr(), PRIVATE_BEARER)
        .await
        .unwrap_err();
    let body = http_body(error);

    assert!(!body.contains(PRIVATE_BEARER));
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["code"],
        "unauthorized"
    );
}
