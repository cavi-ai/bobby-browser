use companion_core::{
    encode_native_message, read_native_message, run_native_host, run_native_host_with_enroll,
    validate_extension_message, validate_server_message, write_native_message, CompanionServer,
    CompanionServerConfig, EnrollHostError, NativeConnectRequest, NativeHostConfig,
    NativeHostEnroll, NativeHostError, NativeReconnectBackoff, MAX_NATIVE_MESSAGE_BYTES,
};
use companion_protocol::{
    ActionRequest, ActionResult, BrowserEngine, BrowserIdentity, BrowserTarget,
    CompanionCapabilities, CompanionEvent, CompanionRequest, InteractionPath, TargetDiscovery,
    TargetKind, PROTOCOL_VERSION,
};
use serde_json::json;
use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{duplex, split, AsyncRead, ReadBuf};
use types::{CommandId, CompanionId, ProfileId};

#[derive(serde::Deserialize)]
struct UrlSecurityFixtures {
    benign: Vec<String>,
    secret: Vec<String>,
}

fn url_security_fixtures() -> UrlSecurityFixtures {
    serde_json::from_str(include_str!("fixtures/extension-url-security.json")).unwrap()
}

struct ChunkedReader {
    bytes: Vec<u8>,
    position: usize,
    chunk_size: usize,
}

impl AsyncRead for ChunkedReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.position == self.bytes.len() {
            return Poll::Ready(Ok(()));
        }
        let count = self
            .chunk_size
            .min(buffer.remaining())
            .min(self.bytes.len() - self.position);
        let end = self.position + count;
        buffer.put_slice(&self.bytes[self.position..end]);
        self.position = end;
        Poll::Ready(Ok(()))
    }
}

fn connect_request() -> NativeConnectRequest {
    NativeConnectRequest {
        protocol_version: PROTOCOL_VERSION,
        companion_id: CompanionId::new(),
        profile_id: ProfileId::new(),
        identity: BrowserIdentity {
            engine: BrowserEngine::Firefox,
            browser_name: "Firefox".into(),
            browser_version: "stable".into(),
            os: "macos".into(),
            profile_label: "default-release".into(),
        },
        capabilities: CompanionCapabilities {
            observe: true,
            navigate: true,
            native_input: false,
            tabs: true,
            frames: true,
            native_dialogs: false,
        },
    }
}

#[test]
fn native_messages_use_little_endian_u32_framing() {
    let frame = encode_native_message(&json!({"kind": "ping"})).unwrap();
    let expected_length = br#"{"kind":"ping"}"#.len() as u32;

    assert_eq!(&frame[..4], &expected_length.to_le_bytes());
    assert_eq!(&frame[4..], br#"{"kind":"ping"}"#);
}

#[tokio::test]
async fn native_message_reader_accepts_partial_reads() {
    let frame = encode_native_message(&json!({"kind": "pong"})).unwrap();
    let mut reader = ChunkedReader {
        bytes: frame,
        position: 0,
        chunk_size: 1,
    };

    let message = read_native_message(&mut reader).await.unwrap().unwrap();

    assert_eq!(message, json!({"kind": "pong"}));
}

#[tokio::test]
async fn oversized_native_length_is_rejected_before_payload_read() {
    let length = (MAX_NATIVE_MESSAGE_BYTES + 1) as u32;
    let mut bytes = &length.to_le_bytes()[..];

    let error = read_native_message(&mut bytes).await.unwrap_err();

    assert!(matches!(
        error,
        NativeHostError::MessageTooLarge { length: actual } if actual == length as usize
    ));
}

#[tokio::test]
async fn malformed_native_json_is_rejected() {
    let payload = b"{";
    let mut frame = (payload.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(payload);
    let mut bytes = &frame[..];

    assert!(matches!(
        read_native_message(&mut bytes).await,
        Err(NativeHostError::InvalidJson)
    ));
}

#[tokio::test]
async fn native_message_codec_round_trips_json() {
    let expected = json!({"kind": "pong"});
    let frame = encode_native_message(&expected).unwrap();
    let mut bytes = &frame[..];

    assert_eq!(
        read_native_message(&mut bytes).await.unwrap(),
        Some(expected)
    );
}

#[test]
fn native_host_owns_pairing_material_and_redacts_it_from_debug() {
    let secret = "pairing-secret-that-must-not-leak";
    let config =
        NativeHostConfig::new("ws://127.0.0.1:49152/v1/companion".parse().unwrap(), secret);

    let request = config.pair_request(connect_request()).unwrap();

    let CompanionRequest::Pair(pair) = request else {
        panic!("expected pair request");
    };
    assert_eq!(pair.pairing_code, secret);
    assert!(!format!("{config:?}").contains(secret));
}

#[test]
fn native_connect_metadata_rejects_recursive_secret_material() {
    let config = NativeHostConfig::new(
        "ws://127.0.0.1:49152/v1/companion".parse().unwrap(),
        "pairing-secret",
    );
    let mut request = connect_request();
    request.identity.profile_label = "Bearer private-token".into();

    assert!(matches!(
        config.pair_request(request),
        Err(NativeHostError::InvalidProtocol)
    ));
}

#[test]
fn native_host_enforces_exact_directional_schemas() {
    assert!(validate_extension_message(json!({"kind": "pong"})).is_ok());
    assert!(validate_extension_message(json!({"kind": "ping"})).is_err());
    assert!(validate_extension_message(json!({
        "kind": "paired",
        "output": {"companionId": CompanionId::new(), "profileId": ProfileId::new()}
    }))
    .is_err());
    assert!(validate_extension_message(json!({
        "kind": "actionCompleted",
        "output": {
            "commandId": "command-1",
            "interactionPath": "extensionApi",
            "output": {"authorization": "Bearer private-token"}
        }
    }))
    .is_err());

    assert!(validate_server_message(json!({"kind": "ping"})).is_ok());
    assert!(validate_server_message(json!({
        "kind": "paired",
        "output": {"companionId": CompanionId::new(), "profileId": ProfileId::new()}
    }))
    .is_ok());
    assert!(validate_server_message(json!({"kind": "pong"})).is_err());
}

#[test]
fn shared_url_security_fixtures_match_the_rust_extension_boundary() {
    let fixtures = url_security_fixtures();
    for url in fixtures.benign {
        let event = json!({
            "kind": "actionCompleted",
            "output": {
                "commandId": CommandId::new(),
                "interactionPath": "extensionApi",
                "output": {"url": url}
            }
        });
        assert!(
            validate_extension_message(event).is_ok(),
            "benign URL was rejected: {url}"
        );
    }
    for url in fixtures.secret {
        let event = json!({
            "kind": "actionCompleted",
            "output": {
                "commandId": CommandId::new(),
                "interactionPath": "extensionApi",
                "output": {"url": url}
            }
        });
        assert!(
            validate_extension_message(event).is_err(),
            "secret URL was accepted: {url}"
        );
    }
}

#[test]
fn native_reconnect_backoff_is_exponential_bounded_and_resettable() {
    let mut backoff = NativeReconnectBackoff::default();
    let delays: Vec<_> = (0..8).map(|_| backoff.next_delay()).collect();

    assert_eq!(
        delays,
        [100, 200, 400, 800, 1_600, 3_200, 5_000, 5_000].map(Duration::from_millis)
    );
    backoff.reset();
    assert_eq!(backoff.next_delay(), Duration::from_millis(100));
}

#[tokio::test]
async fn native_host_keeps_pairing_material_out_of_the_extension_channel() {
    let server = CompanionServer::bind_loopback(CompanionServerConfig {
        bind_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        pairing_code_ttl: Duration::from_secs(60),
        attachment_ttl: Duration::from_secs(300),
    })
    .await
    .unwrap();
    let pairing_code = server.registry().issue_pairing_code().await;
    let config = NativeHostConfig::new(
        format!("ws://{}/v1/companion", server.local_addr()),
        pairing_code.clone(),
    );
    let connect = json!({"kind": "pair", "input": connect_request()});
    assert!(!serde_json::to_string(&connect)
        .unwrap()
        .contains(&pairing_code));

    let (host_stream, mut extension_stream) = duplex(2 * MAX_NATIVE_MESSAGE_BYTES);
    let (host_reader, host_writer) = split(host_stream);
    let host = tokio::spawn(run_native_host(host_reader, host_writer, config));

    write_native_message(&mut extension_stream, &connect)
        .await
        .unwrap();
    let paired = read_native_message(&mut extension_stream)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(paired["kind"], "paired");
    assert!(!serde_json::to_string(&paired)
        .unwrap()
        .contains(&pairing_code));

    server.disconnect_clients();
    let resumed = tokio::time::timeout(
        Duration::from_secs(2),
        read_native_message(&mut extension_stream),
    )
    .await
    .expect("native host must reconnect after a live server disconnect")
    .unwrap()
    .unwrap();
    assert_eq!(resumed["kind"], "paired");
    assert!(resumed["output"].get("reconnectCredential").is_none());
    assert!(!serde_json::to_string(&resumed)
        .unwrap()
        .contains(&pairing_code));

    drop(extension_stream);
    host.await.unwrap().unwrap();
}

#[tokio::test]
async fn rust_request_crosses_server_native_and_extension_and_event_returns_without_close() {
    let server = CompanionServer::bind_loopback(CompanionServerConfig {
        bind_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        pairing_code_ttl: Duration::from_secs(60),
        attachment_ttl: Duration::from_secs(300),
    })
    .await
    .unwrap();
    let pairing_code = server.registry().issue_pairing_code().await;
    let config = NativeHostConfig::new(
        format!("ws://{}/v1/companion", server.local_addr()),
        pairing_code,
    );
    let connect_request = connect_request();
    let profile_id = connect_request.profile_id.clone();
    let connect = json!({"kind": "pair", "input": connect_request});
    let (host_stream, mut extension_stream) = duplex(2 * MAX_NATIVE_MESSAGE_BYTES);
    let (host_reader, host_writer) = split(host_stream);
    let host = tokio::spawn(run_native_host(host_reader, host_writer, config));

    write_native_message(&mut extension_stream, &connect)
        .await
        .unwrap();
    assert_eq!(
        read_native_message(&mut extension_stream)
            .await
            .unwrap()
            .unwrap()["kind"],
        "paired"
    );

    let target = BrowserTarget {
        target_id: "opaque-simulated-subframe".into(),
        kind: TargetKind::Frame,
    };
    let discovery = CompanionEvent::TargetsDiscovered(TargetDiscovery {
        protocol_version: PROTOCOL_VERSION,
        profile_id: profile_id.clone(),
        targets: vec![target.clone()],
    });
    write_native_message(
        &mut extension_stream,
        &serde_json::to_value(discovery).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        server
            .wait_for_discovery(&profile_id, Duration::from_secs(1))
            .await
            .unwrap(),
        vec![target]
    );

    let grant = server.grant_discovered_targets(&profile_id).await.unwrap();
    let wire_grant: CompanionRequest = serde_json::from_value(
        read_native_message(&mut extension_stream)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(wire_grant, CompanionRequest::Grant(grant.clone()));

    let command_id = CommandId::new();
    let action = ActionRequest {
        protocol_version: PROTOCOL_VERSION,
        attachment_id: grant.attachment_id,
        command_id: command_id.clone(),
        page_id: grant.pages[0].page_id.clone(),
        operation: "observe".into(),
        input: json!({}),
        deadline_unix_ms: 4_102_444_800_000,
    };
    let expected_action = action.clone();
    let completed = CompanionEvent::ActionCompleted(ActionResult {
        command_id,
        interaction_path: InteractionPath::ExtensionApi,
        output: json!({"visibleText": "ready"}),
    });
    let completed_for_extension = completed.clone();
    let extension = async {
        let wire_request: CompanionRequest = serde_json::from_value(
            read_native_message(&mut extension_stream)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(wire_request, CompanionRequest::Action(expected_action));
        write_native_message(
            &mut extension_stream,
            &serde_json::to_value(completed_for_extension).unwrap(),
        )
        .await
        .unwrap();
    };
    let (result, ()) = tokio::join!(server.dispatch_action(action), extension);
    assert_eq!(result.unwrap(), completed);

    server
        .send_request(&profile_id, CompanionRequest::Ping)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_value::<CompanionRequest>(
            read_native_message(&mut extension_stream)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap(),
        CompanionRequest::Ping
    );
    write_native_message(
        &mut extension_stream,
        &serde_json::to_value(CompanionEvent::Pong).unwrap(),
    )
    .await
    .unwrap();

    drop(extension_stream);
    host.await.unwrap().unwrap();
}

#[tokio::test]
async fn revoked_reconnect_credential_stops_the_native_host() {
    let server = CompanionServer::bind_loopback(CompanionServerConfig {
        bind_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        pairing_code_ttl: Duration::from_secs(60),
        attachment_ttl: Duration::from_secs(300),
    })
    .await
    .unwrap();
    let pairing_code = server.registry().issue_pairing_code().await;
    let config = NativeHostConfig::new(
        format!("ws://{}/v1/companion", server.local_addr()),
        pairing_code,
    );
    let request = connect_request();
    let companion_id = request.companion_id.clone();
    let connect = json!({"kind": "pair", "input": request});

    let (host_stream, mut extension_stream) = duplex(2 * MAX_NATIVE_MESSAGE_BYTES);
    let (host_reader, host_writer) = split(host_stream);
    let host = tokio::spawn(run_native_host(host_reader, host_writer, config));

    write_native_message(&mut extension_stream, &connect)
        .await
        .unwrap();
    let paired = read_native_message(&mut extension_stream)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(paired["kind"], "paired");

    server.registry().revoke(&companion_id).await.unwrap();
    server.disconnect_clients();

    let terminal = tokio::time::timeout(
        Duration::from_secs(2),
        read_native_message(&mut extension_stream),
    )
    .await
    .expect("native host must send terminal auth status before exit")
    .unwrap()
    .unwrap();
    assert_eq!(
        terminal,
        json!({"kind": "nativeStatus", "output": {"state": "invalidAuth"}})
    );

    let result = tokio::time::timeout(Duration::from_secs(2), host)
        .await
        .expect("revoked reconnect credentials must not retry forever")
        .unwrap();
    assert!(matches!(
        result,
        Err(NativeHostError::InvalidPairingMaterial)
    ));
}

#[tokio::test]
async fn native_eof_cancels_connection_attempts_and_backoff_promptly() {
    let server = CompanionServer::bind_loopback(CompanionServerConfig {
        bind_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        pairing_code_ttl: Duration::from_secs(60),
        attachment_ttl: Duration::from_secs(300),
    })
    .await
    .unwrap();
    let pairing_code = server.registry().issue_pairing_code().await;
    let config = NativeHostConfig::new(
        format!("ws://{}/v1/companion", server.local_addr()),
        pairing_code,
    );
    let connect = json!({"kind": "pair", "input": connect_request()});
    let (host_stream, mut extension_stream) = duplex(2 * MAX_NATIVE_MESSAGE_BYTES);
    let (host_reader, host_writer) = split(host_stream);
    let host = tokio::spawn(run_native_host(host_reader, host_writer, config));

    write_native_message(&mut extension_stream, &connect)
        .await
        .unwrap();
    assert_eq!(
        read_native_message(&mut extension_stream)
            .await
            .unwrap()
            .unwrap()["kind"],
        "paired"
    );
    drop(server);
    tokio::time::sleep(Duration::from_millis(75)).await;
    drop(extension_stream);

    let result = tokio::time::timeout(Duration::from_millis(250), host)
        .await
        .expect("native EOF must cancel connection attempts and backoff")
        .unwrap();
    assert!(result.is_ok());
}


#[test]
fn enroll_profile_request_decodes_empty_input() {
    let value = json!({ "kind": "enrollProfile", "input": {} });
    let request = companion_core::decode_native_request(value).expect("enrollProfile");
    assert!(matches!(
        request,
        companion_core::NativeRequest::EnrollProfile(_)
    ));
}

#[test]
fn enroll_profile_request_rejects_secret_fields() {
    let value = json!({
        "kind": "enrollProfile",
        "input": { "pairingCode": "nope" }
    });
    assert!(matches!(
        companion_core::decode_native_request(value),
        Err(NativeHostError::InvalidProtocol)
    ));
}

struct FakeEnroll {
    config: NativeHostConfig,
    completed: Arc<AtomicBool>,
}

impl NativeHostEnroll for FakeEnroll {
    fn enroll_and_wait_for_pair(
        &self,
        _pair: NativeConnectRequest,
    ) -> impl Future<Output = Result<NativeHostConfig, EnrollHostError>> + Send {
        let config = self.config.clone();
        async move { Ok(config) }
    }

    fn complete_enrollment(
        &self,
        _pair: &NativeConnectRequest,
    ) -> impl Future<Output = Result<(), EnrollHostError>> + Send {
        let completed = Arc::clone(&self.completed);
        async move {
            completed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }
}

#[tokio::test]
async fn enroll_profile_then_pair_emits_enroll_ok_via_enroll_trait() {
    let server = CompanionServer::bind_loopback(CompanionServerConfig {
        bind_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        pairing_code_ttl: Duration::from_secs(60),
        attachment_ttl: Duration::from_secs(300),
    })
    .await
    .unwrap();
    let pairing_code = server.registry().issue_pairing_code().await;
    let config = NativeHostConfig::new(
        format!("ws://{}/v1/companion", server.local_addr()),
        pairing_code.clone(),
    );
    let completed = Arc::new(AtomicBool::new(false));
    let enroll = FakeEnroll {
        config,
        completed: Arc::clone(&completed),
    };
    let connect = connect_request();
    let enroll_frame = json!({ "kind": "enrollProfile", "input": {} });
    let pair_frame = json!({ "kind": "pair", "input": connect });

    let (host_stream, mut extension_stream) = duplex(2 * MAX_NATIVE_MESSAGE_BYTES);
    let (host_reader, host_writer) = split(host_stream);
    let host = tokio::spawn(run_native_host_with_enroll(
        host_reader,
        host_writer,
        None,
        Some(enroll),
    ));

    write_native_message(&mut extension_stream, &enroll_frame)
        .await
        .unwrap();
    write_native_message(&mut extension_stream, &pair_frame)
        .await
        .unwrap();

    let paired = read_native_message(&mut extension_stream)
        .await
        .unwrap()
        .unwrap();
    // complete_enrollment runs before paired is written to the extension.
    assert!(completed.load(Ordering::SeqCst));
    assert_eq!(paired["kind"], "paired");
    assert!(!serde_json::to_string(&paired)
        .unwrap()
        .contains(&pairing_code));

    let status = read_native_message(&mut extension_stream)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        status,
        json!({ "kind": "nativeStatus", "output": { "state": "enrollOk" } })
    );

    let result = tokio::time::timeout(Duration::from_secs(2), host)
        .await
        .expect("enroll path must exit after enrollOk")
        .unwrap();
    assert!(result.is_ok());
}

#[tokio::test]
async fn enroll_persist_failure_does_not_emit_paired() {
    struct PersistFailEnroll {
        config: NativeHostConfig,
    }
    impl NativeHostEnroll for PersistFailEnroll {
        fn enroll_and_wait_for_pair(
            &self,
            _pair: NativeConnectRequest,
        ) -> impl Future<Output = Result<NativeHostConfig, EnrollHostError>> + Send {
            let config = self.config.clone();
            async move { Ok(config) }
        }

        fn complete_enrollment(
            &self,
            _pair: &NativeConnectRequest,
        ) -> impl Future<Output = Result<(), EnrollHostError>> + Send {
            async { Err(EnrollHostError::ListenerUnavailable) }
        }
    }

    let server = CompanionServer::bind_loopback(CompanionServerConfig {
        bind_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        pairing_code_ttl: Duration::from_secs(60),
        attachment_ttl: Duration::from_secs(300),
    })
    .await
    .unwrap();
    let pairing_code = server.registry().issue_pairing_code().await;
    let config = NativeHostConfig::new(
        format!("ws://{}/v1/companion", server.local_addr()),
        pairing_code,
    );
    let (host_stream, mut extension_stream) = duplex(2 * MAX_NATIVE_MESSAGE_BYTES);
    let (host_reader, host_writer) = split(host_stream);
    let host = tokio::spawn(run_native_host_with_enroll(
        host_reader,
        host_writer,
        None,
        Some(PersistFailEnroll { config }),
    ));

    write_native_message(
        &mut extension_stream,
        &json!({ "kind": "enrollProfile", "input": {} }),
    )
    .await
    .unwrap();
    write_native_message(
        &mut extension_stream,
        &json!({ "kind": "pair", "input": connect_request() }),
    )
    .await
    .unwrap();

    let status = read_native_message(&mut extension_stream)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        status,
        json!({
            "kind": "nativeStatus",
            "output": { "state": "enrollFailed", "code": "listenerUnavailable" }
        })
    );
    assert_ne!(status["kind"], "paired");
    let _ = host.await;
}

#[tokio::test]
async fn enroll_profile_reports_defaults_missing_from_enroll_trait() {
    struct FailingEnroll;
    impl NativeHostEnroll for FailingEnroll {
        fn enroll_and_wait_for_pair(
            &self,
            _pair: NativeConnectRequest,
        ) -> impl Future<Output = Result<NativeHostConfig, EnrollHostError>> + Send {
            async { Err(EnrollHostError::DefaultsMissing) }
        }

        fn complete_enrollment(
            &self,
            _pair: &NativeConnectRequest,
        ) -> impl Future<Output = Result<(), EnrollHostError>> + Send {
            async { Ok(()) }
        }
    }

    let (host_stream, mut extension_stream) = duplex(2 * MAX_NATIVE_MESSAGE_BYTES);
    let (host_reader, host_writer) = split(host_stream);
    let host = tokio::spawn(run_native_host_with_enroll(
        host_reader,
        host_writer,
        None,
        Some(FailingEnroll),
    ));

    write_native_message(
        &mut extension_stream,
        &json!({ "kind": "enrollProfile", "input": {} }),
    )
    .await
    .unwrap();
    write_native_message(
        &mut extension_stream,
        &json!({ "kind": "pair", "input": connect_request() }),
    )
    .await
    .unwrap();

    let status = read_native_message(&mut extension_stream)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        status,
        json!({
            "kind": "nativeStatus",
            "output": { "state": "enrollFailed", "code": "defaultsMissing" }
        })
    );
    host.await.unwrap().unwrap();
}
