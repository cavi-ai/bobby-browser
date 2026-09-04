use companion_core::{CompanionServer, CompanionServerConfig, CompanionServerError, PairingInput};
use companion_protocol::{
    ActionRequest, ActionResult, BrowserTarget, CompanionEvent, CompanionRequest, InteractionPath,
    PairRequest, TargetDiscovery, TargetKind, PROTOCOL_VERSION,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::{
    net::SocketAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::StatusCode, Error, Message},
    MaybeTlsStream, WebSocketStream,
};

const PRIVATE_BEARER: &str = "private-bearer-that-must-never-appear";
const PRIVATE_FRAME_MARKER: &str = "private-frame-body-that-must-never-appear";

type ClientSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

fn now_unix_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

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

async fn send_event(socket: &mut ClientSocket, event: &CompanionEvent) {
    socket
        .send(Message::Text(serde_json::to_string(event).unwrap().into()))
        .await
        .unwrap();
}

async fn receive_request(socket: &mut ClientSocket) -> CompanionRequest {
    loop {
        match socket.next().await.unwrap().unwrap() {
            Message::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
            Message::Ping(payload) => socket.send(Message::Pong(payload)).await.unwrap(),
            other => panic!("expected companion request, got {other:?}"),
        }
    }
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
    let CompanionEvent::Paired { profile_id, .. } = receive_event(&mut resumed).await else {
        panic!("expected paired event");
    };
    server
        .send_request(&profile_id, CompanionRequest::Ping)
        .await
        .unwrap();
    assert_eq!(receive_request(&mut resumed).await, CompanionRequest::Ping);
    send_event(&mut resumed, &CompanionEvent::Pong).await;

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
    let CompanionEvent::Paired { profile_id, .. } = pair(&mut socket, code).await else {
        panic!("expected paired event");
    };

    server
        .send_request(&profile_id, CompanionRequest::Ping)
        .await
        .unwrap();
    assert_eq!(receive_request(&mut socket).await, CompanionRequest::Ping);
    send_event(&mut socket, &CompanionEvent::Pong).await;

    server
        .send_request(&profile_id, CompanionRequest::Ping)
        .await
        .unwrap();
    assert_eq!(receive_request(&mut socket).await, CompanionRequest::Ping);
}

#[tokio::test]
async fn post_pair_client_requests_are_rejected_as_the_wrong_direction() {
    let server = CompanionServer::bind_loopback(loopback_config())
        .await
        .unwrap();
    let code = server.registry().issue_pairing_code().await;
    let mut socket = connect_with_bearer(server.local_addr(), &code)
        .await
        .unwrap();
    pair(&mut socket, code).await;

    send_request(&mut socket, &CompanionRequest::Ping).await;

    let Message::Text(body) = socket.next().await.unwrap().unwrap() else {
        panic!("expected typed wrong-direction error");
    };
    let error: Value = serde_json::from_str(body.as_str()).unwrap();
    assert_eq!(error["code"], "invalidEvent");
    assert_eq!(error["message"], "event must be strict companion JSON");
}

#[tokio::test]
async fn discovery_requires_an_explicit_real_grant_before_actions_are_dispatched() {
    let server = CompanionServer::bind_loopback(loopback_config())
        .await
        .unwrap();
    let code = server.registry().issue_pairing_code().await;
    let input = PairingInput::firefox(code.clone());
    let profile_id = input.profile_id.clone();
    let mut socket = connect_with_bearer(server.local_addr(), &code)
        .await
        .unwrap();
    send_request(
        &mut socket,
        &CompanionRequest::Pair(PairRequest {
            protocol_version: PROTOCOL_VERSION,
            pairing_code: code,
            companion_id: input.companion_id,
            profile_id: profile_id.clone(),
            identity: input.identity,
            capabilities: input.capabilities,
        }),
    )
    .await;
    assert!(matches!(
        receive_event(&mut socket).await,
        CompanionEvent::Paired { .. }
    ));

    let target = BrowserTarget {
        target_id: "opaque-subframe-target".into(),
        kind: TargetKind::Frame,
    };
    send_event(
        &mut socket,
        &CompanionEvent::TargetsDiscovered(TargetDiscovery {
            protocol_version: PROTOCOL_VERSION,
            profile_id: profile_id.clone(),
            targets: vec![target.clone()],
        }),
    )
    .await;
    let discovered = server
        .wait_for_discovery(&profile_id, Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(discovered, vec![target]);
    assert!(server.active_grant(&profile_id).await.is_none());

    let grant = server.grant_discovered_targets(&profile_id).await.unwrap();
    let CompanionRequest::Grant(wire_grant) = receive_request(&mut socket).await else {
        panic!("expected explicit attachment grant");
    };
    assert_eq!(wire_grant, grant);
    assert_eq!(grant.attachment_id.0.get_version_num(), 4);
    assert_eq!(grant.pages[0].page_id.0.get_version_num(), 4);
    let renewed = server.renew_grant(&grant.attachment_id).await.unwrap();
    let CompanionRequest::Grant(wire_renewal) = receive_request(&mut socket).await else {
        panic!("expected explicit attachment renewal");
    };
    assert_eq!(wire_renewal, renewed);
    assert_eq!(renewed.attachment_id, grant.attachment_id);
    assert_eq!(renewed.pages, grant.pages);
    assert!(renewed.expires_at_unix_ms >= grant.expires_at_unix_ms);
    let grant = renewed;

    let command_id = types::CommandId::new();
    let action = ActionRequest {
        protocol_version: PROTOCOL_VERSION,
        attachment_id: grant.attachment_id.clone(),
        command_id: command_id.clone(),
        page_id: grant.pages[0].page_id.clone(),
        operation: "observe".into(),
        input: serde_json::json!({}),
        deadline_unix_ms: now_unix_ms() + 5_000,
    };
    let expected_action = action.clone();
    let completed = CompanionEvent::ActionCompleted(ActionResult {
        command_id,
        interaction_path: InteractionPath::ExtensionApi,
        output: serde_json::json!({"observed": true}),
    });
    let completed_for_extension = completed.clone();
    let extension = async {
        assert_eq!(
            receive_request(&mut socket).await,
            CompanionRequest::Action(expected_action)
        );
        send_event(&mut socket, &completed_for_extension).await;
    };
    let (dispatched, ()) = tokio::join!(server.dispatch_action(action), extension);
    assert_eq!(dispatched.unwrap(), completed);

    server
        .send_request(&profile_id, CompanionRequest::Ping)
        .await
        .unwrap();
    assert_eq!(receive_request(&mut socket).await, CompanionRequest::Ping);
}

#[tokio::test]
async fn dispatch_rejects_mismatched_attachment_page_and_profile_bindings() {
    let server = CompanionServer::bind_loopback(loopback_config())
        .await
        .unwrap();
    let code = server.registry().issue_pairing_code().await;
    let input = PairingInput::firefox(code.clone());
    let profile_id = input.profile_id.clone();
    let mut socket = connect_with_bearer(server.local_addr(), &code)
        .await
        .unwrap();
    send_request(
        &mut socket,
        &CompanionRequest::Pair(PairRequest {
            protocol_version: PROTOCOL_VERSION,
            pairing_code: code,
            companion_id: input.companion_id,
            profile_id: profile_id.clone(),
            identity: input.identity,
            capabilities: input.capabilities,
        }),
    )
    .await;
    receive_event(&mut socket).await;
    send_event(
        &mut socket,
        &CompanionEvent::TargetsDiscovered(TargetDiscovery {
            protocol_version: PROTOCOL_VERSION,
            profile_id: profile_id.clone(),
            targets: vec![BrowserTarget {
                target_id: "trusted-target".into(),
                kind: TargetKind::Page,
            }],
        }),
    )
    .await;
    server
        .wait_for_discovery(&profile_id, Duration::from_secs(1))
        .await
        .unwrap();
    let grant = server.grant_discovered_targets(&profile_id).await.unwrap();
    let _ = receive_request(&mut socket).await;
    let base = ActionRequest {
        protocol_version: PROTOCOL_VERSION,
        attachment_id: grant.attachment_id.clone(),
        command_id: types::CommandId::new(),
        page_id: grant.pages[0].page_id.clone(),
        operation: "observe".into(),
        input: serde_json::json!({}),
        deadline_unix_ms: now_unix_ms() + 5_000,
    };

    let mut wrong_attachment = base.clone();
    wrong_attachment.attachment_id = types::AttachmentId::new();
    assert!(server.dispatch_action(wrong_attachment).await.is_err());
    let mut wrong_page = base.clone();
    wrong_page.page_id = types::PageId::new();
    assert!(server.dispatch_action(wrong_page).await.is_err());

    send_event(
        &mut socket,
        &CompanionEvent::TargetsDiscovered(TargetDiscovery {
            protocol_version: PROTOCOL_VERSION,
            profile_id: types::ProfileId::new(),
            targets: vec![],
        }),
    )
    .await;
    assert!(matches!(
        socket.next().await.unwrap().unwrap(),
        Message::Text(_) | Message::Close(_)
    ));
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
async fn wait_for_discovery_accepts_a_paired_session_without_targets() {
    let server = CompanionServer::bind_loopback(loopback_config())
        .await
        .unwrap();
    let code = server.registry().issue_pairing_code().await;
    let input = PairingInput::firefox(code.clone());
    let profile_id = input.profile_id.clone();
    let mut socket = connect_with_bearer(server.local_addr(), &code)
        .await
        .unwrap();
    send_request(
        &mut socket,
        &CompanionRequest::Pair(PairRequest {
            protocol_version: PROTOCOL_VERSION,
            pairing_code: code,
            companion_id: input.companion_id,
            profile_id: profile_id.clone(),
            identity: input.identity,
            capabilities: input.capabilities,
        }),
    )
    .await;
    assert!(matches!(
        receive_event(&mut socket).await,
        CompanionEvent::Paired { .. }
    ));
    let discovered = server
        .wait_for_discovery(&profile_id, Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(discovered, Vec::<BrowserTarget>::new());
    let grant = server.grant_discovered_targets(&profile_id).await.unwrap();
    assert!(grant.pages.is_empty());
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
