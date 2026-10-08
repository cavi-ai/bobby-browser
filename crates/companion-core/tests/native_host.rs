use companion_core::{
    encode_native_message, read_native_message, run_native_host, run_native_host_with_enroll,
    validate_extension_message, validate_server_message, write_native_message, CompanionServer,
    CompanionServerConfig, EnrollFinalize, EnrollHostError, NativeConnectRequest, NativeHostConfig,
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
#[serde(rename_all = "camelCase")]
struct UrlSecurityFixtures {
    benign: Vec<String>,
    secret: Vec<String>,
    text_benign: Vec<String>,
    text_secret: Vec<String>,
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
        extension_build_id: None,
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

/// Free text (page text, names, selectors) is not URL-parsed as a whole;
/// only the absolute URLs embedded in it meet the URL rules.
#[test]
fn shared_free_text_fixtures_match_the_rust_extension_boundary() {
    let fixtures = url_security_fixtures();
    let event = |text: &str| {
        json!({
            "kind": "actionCompleted",
            "output": {
                "commandId": CommandId::new(),
                "interactionPath": "extensionApi",
                "output": {"nodes": [{"role": "StaticText", "name": text}], "selector": text}
            }
        })
    };
    for text in fixtures.text_benign {
        assert!(
            validate_extension_message(event(&text)).is_ok(),
            "benign text was rejected: {text}"
        );
    }
    for text in fixtures.text_secret {
        assert!(
            validate_extension_message(event(&text)).is_err(),
            "secret text was accepted: {text}"
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

/// The server spends a pairing code on the upgrade, so a descriptor notice
/// that names the same endpoint and owner must not abandon the attempt: a
/// retry with the spent code is refused and the host stops on invalidAuth.
#[tokio::test]
async fn a_descriptor_notice_without_a_new_owner_keeps_the_pairing_attempt() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::{oneshot, watch};

    let server = CompanionServer::bind_loopback(CompanionServerConfig {
        bind_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        pairing_code_ttl: Duration::from_secs(60),
        attachment_ttl: Duration::from_secs(300),
    })
    .await
    .unwrap();
    let pairing_code = server.registry().issue_pairing_code().await;
    let upstream = server.local_addr();

    // Holds the server's answer to the first connection until released;
    // later connections pass straight through.
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/v1/companion", proxy.local_addr().unwrap());
    let (answered_tx, answered) = oneshot::channel::<()>();
    let (release, release_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let mut hold = Some((answered_tx, release_rx));
        while let Ok((client, _)) = proxy.accept().await {
            let upstream = TcpStream::connect(upstream).await.unwrap();
            let held = hold.take();
            tokio::spawn(async move {
                let (mut client_read, mut client_write) = client.into_split();
                let (mut upstream_read, mut upstream_write) = upstream.into_split();
                tokio::spawn(async move {
                    let _ = tokio::io::copy(&mut client_read, &mut upstream_write).await;
                });
                if let Some((answered, release)) = held {
                    let mut first = [0_u8; 4096];
                    let count = upstream_read.read(&mut first).await.unwrap_or(0);
                    let _ = answered.send(());
                    let _ = release.await;
                    if client_write.write_all(&first[..count]).await.is_err() {
                        return;
                    }
                }
                let _ = tokio::io::copy(&mut upstream_read, &mut client_write).await;
            });
        }
    });

    let (notice, changes) = watch::channel(0_u64);
    let same_endpoint = endpoint.clone();
    let same_code = pairing_code.clone();
    let config =
        NativeHostConfig::new(endpoint, pairing_code).with_config_refresh(changes, move || {
            Some(NativeHostConfig::new(
                same_endpoint.clone(),
                same_code.clone(),
            ))
        });
    let (host_stream, mut extension_stream) = duplex(2 * MAX_NATIVE_MESSAGE_BYTES);
    let (host_reader, host_writer) = split(host_stream);
    let host = tokio::spawn(run_native_host(host_reader, host_writer, config));
    write_native_message(
        &mut extension_stream,
        &json!({"kind": "pair", "input": connect_request()}),
    )
    .await
    .unwrap();

    tokio::time::timeout(Duration::from_secs(5), answered)
        .await
        .unwrap()
        .unwrap();
    notice.send_modify(|version| *version += 1);
    tokio::time::sleep(Duration::from_millis(100)).await;
    release.send(()).unwrap();

    let message = tokio::time::timeout(
        Duration::from_secs(5),
        read_native_message(&mut extension_stream),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert_eq!(message["kind"], "paired", "{message}");
    drop(extension_stream);
    host.await.unwrap().unwrap();
}

#[tokio::test]
async fn the_native_host_forwards_the_extension_build_and_relays_reload() {
    const BUILD: &str = "0123456789abcdef0123456789abcdef";
    let server = CompanionServer::bind_loopback(CompanionServerConfig {
        bind_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        pairing_code_ttl: Duration::from_secs(60),
        attachment_ttl: Duration::from_secs(300),
    })
    .await
    .unwrap();
    let config = NativeHostConfig::new(
        format!("ws://{}/v1/companion", server.local_addr()),
        server.registry().issue_pairing_code().await,
    );
    let mut connect_request = connect_request();
    connect_request.extension_build_id = Some(BUILD.into());
    let profile_id = connect_request.profile_id.clone();
    let (host_stream, mut extension_stream) = duplex(2 * MAX_NATIVE_MESSAGE_BYTES);
    let (host_reader, host_writer) = split(host_stream);
    let host = tokio::spawn(run_native_host(host_reader, host_writer, config));
    write_native_message(
        &mut extension_stream,
        &json!({"kind": "pair", "input": connect_request}),
    )
    .await
    .unwrap();
    assert_eq!(
        read_native_message(&mut extension_stream)
            .await
            .unwrap()
            .unwrap()["kind"],
        "paired"
    );

    let connection = server.extension_connection(&profile_id).await.unwrap();
    assert_eq!(connection.build_id(), Some(BUILD));
    server
        .send_request(&profile_id, CompanionRequest::Reload)
        .await
        .unwrap();
    assert_eq!(
        read_native_message(&mut extension_stream)
            .await
            .unwrap()
            .unwrap(),
        json!({"kind": "reload"})
    );

    drop(extension_stream);
    host.await.unwrap().unwrap();
}

#[test]
fn a_malformed_extension_build_is_refused_by_the_native_host() {
    let config = NativeHostConfig::new("ws://127.0.0.1:9/v1/companion".into(), "a".repeat(32));
    let mut request = connect_request();
    request.extension_build_id = Some("@@BOBBY_EXTENSION_BUILD_ID@@".into());
    assert!(config.pair_request(request.clone()).is_err());
    request.extension_build_id = Some("0123456789abcdef0123456789abcdef".into());
    assert!(config.pair_request(request).is_ok());
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
    let store_dir = std::env::temp_dir().join(format!("native-host-store-{}", CommandId::new().0));
    std::fs::create_dir_all(&store_dir).unwrap();
    let store = store_dir.join("firefox-native-host-credential.json");
    let config = NativeHostConfig::new(
        format!("ws://{}/v1/companion", server.local_addr()),
        pairing_code,
    )
    .with_credential_store(store.clone());
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
    assert!(store.exists(), "pairing stores the reconnect credential");

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
    assert!(!store.exists(), "a refused credential is deleted");
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
        std::future::ready(Ok(self.config.clone()))
    }

    fn complete_enrollment(
        &self,
        _pair: &NativeConnectRequest,
    ) -> impl Future<Output = Result<EnrollFinalize, EnrollHostError>> + Send {
        self.completed.store(true, Ordering::SeqCst);
        std::future::ready(Ok(EnrollFinalize::ReleaseListener))
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
            std::future::ready(Ok(self.config.clone()))
        }

        fn complete_enrollment(
            &self,
            _pair: &NativeConnectRequest,
        ) -> impl Future<Output = Result<EnrollFinalize, EnrollHostError>> + Send {
            std::future::ready(Err(EnrollHostError::ListenerUnavailable))
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
    let result = host.await.unwrap();
    assert!(
        result.is_ok(),
        "enrollFailed must exit cleanly, not as InvalidProtocol: {result:?}"
    );
}

#[tokio::test]
async fn enroll_profile_reports_defaults_missing_from_enroll_trait() {
    struct FailingEnroll;
    impl NativeHostEnroll for FailingEnroll {
        fn enroll_and_wait_for_pair(
            &self,
            _pair: NativeConnectRequest,
        ) -> impl Future<Output = Result<NativeHostConfig, EnrollHostError>> + Send {
            std::future::ready(Err(EnrollHostError::DefaultsMissing))
        }

        fn complete_enrollment(
            &self,
            _pair: &NativeConnectRequest,
        ) -> impl Future<Output = Result<EnrollFinalize, EnrollHostError>> + Send {
            std::future::ready(Ok(EnrollFinalize::ReleaseListener))
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

#[tokio::test]
async fn enroll_keep_relay_leaves_host_running_after_enroll_ok() {
    struct KeepRelayEnroll {
        config: NativeHostConfig,
    }
    impl NativeHostEnroll for KeepRelayEnroll {
        fn enroll_and_wait_for_pair(
            &self,
            _pair: NativeConnectRequest,
        ) -> impl Future<Output = Result<NativeHostConfig, EnrollHostError>> + Send {
            std::future::ready(Ok(self.config.clone()))
        }

        fn complete_enrollment(
            &self,
            _pair: &NativeConnectRequest,
        ) -> impl Future<Output = Result<EnrollFinalize, EnrollHostError>> + Send {
            std::future::ready(Ok(EnrollFinalize::KeepRelay))
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
    let mut host = tokio::spawn(run_native_host_with_enroll(
        host_reader,
        host_writer,
        None,
        Some(KeepRelayEnroll { config }),
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

    let paired = read_native_message(&mut extension_stream)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(paired["kind"], "paired");
    let status = read_native_message(&mut extension_stream)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        status,
        json!({ "kind": "nativeStatus", "output": { "state": "enrollOk" } })
    );

    // Must not double-write the initial paired frame after enrollOk.
    let third = tokio::time::timeout(
        Duration::from_millis(100),
        read_native_message(&mut extension_stream),
    )
    .await;
    assert!(
        third.is_err()
            || matches!(
                third.as_ref().ok().and_then(|r| r.as_ref().ok()),
                Some(None)
            ),
        "KeepRelay must not emit a second paired after enrollOk; got {third:?}"
    );

    // Live path must not exit after enrollOk — drop the extension side to finish.
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut host)
            .await
            .is_err(),
        "KeepRelay enroll must leave the native host running"
    );
    drop(extension_stream);
    let _ = tokio::time::timeout(Duration::from_secs(2), host)
        .await
        .expect("host exits after native close")
        .unwrap();
}

/// Regression: port reassignment must move an already-paired native relay,
/// without requiring Firefox to restart or the operator to pair again.
#[tokio::test]
async fn native_relay_follows_reassigned_endpoint_while_old_server_stays_live() {
    let first = CompanionServer::bind_loopback(CompanionServerConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        pairing_code_ttl: Duration::from_secs(60),
        attachment_ttl: Duration::from_secs(60),
    })
    .await
    .unwrap();
    let second = CompanionServer::bind_loopback(CompanionServerConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        pairing_code_ttl: Duration::from_secs(60),
        attachment_ttl: Duration::from_secs(60),
    })
    .await
    .unwrap();
    let replacement = Arc::new(std::sync::Mutex::new(None::<NativeHostConfig>));
    let refresh = Arc::clone(&replacement);
    let (changed, changes) = tokio::sync::watch::channel(0_u64);
    let config = NativeHostConfig::new(
        format!("ws://{}/v1/companion", first.local_addr()),
        first.registry().issue_pairing_code().await,
    )
    .with_config_refresh(changes, move || refresh.lock().unwrap().clone());
    let (host_stream, mut extension) = duplex(2 * MAX_NATIVE_MESSAGE_BYTES);
    let (reader, writer) = split(host_stream);
    let host = tokio::spawn(run_native_host(reader, writer, config));
    let connect = connect_request();
    let profile_id = connect.profile_id.clone();
    write_native_message(&mut extension, &json!({"kind":"pair","input":connect}))
        .await
        .unwrap();
    assert_eq!(
        read_native_message(&mut extension).await.unwrap().unwrap()["kind"],
        "paired"
    );
    *replacement.lock().unwrap() = Some(NativeHostConfig::new(
        format!("ws://{}/v1/companion", second.local_addr()),
        second.registry().issue_pairing_code().await,
    ));
    changed.send_modify(|version| *version += 1);
    let paired = tokio::time::timeout(Duration::from_secs(3), read_native_message(&mut extension))
        .await
        .expect("relay must discover port reassignment")
        .unwrap()
        .unwrap();
    assert_eq!(paired["kind"], "paired");
    let target = BrowserTarget {
        target_id: "reassigned-target".into(),
        kind: TargetKind::Frame,
    };
    write_native_message(
        &mut extension,
        &serde_json::to_value(CompanionEvent::TargetsDiscovered(TargetDiscovery {
            protocol_version: PROTOCOL_VERSION,
            profile_id: profile_id.clone(),
            targets: vec![target.clone()],
        }))
        .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        second
            .wait_for_discovery(&profile_id, Duration::from_secs(1))
            .await
            .unwrap(),
        vec![target]
    );
    assert!(tokio::net::TcpStream::connect(first.local_addr())
        .await
        .is_ok());
    drop(extension);
    host.await.unwrap().unwrap();
}

#[tokio::test]
async fn endpoint_reassignment_interrupts_a_stalled_websocket_handshake() {
    let held = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let second = CompanionServer::bind_loopback(CompanionServerConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        pairing_code_ttl: Duration::from_secs(60),
        attachment_ttl: Duration::from_secs(60),
    })
    .await
    .unwrap();
    let replacement = Arc::new(std::sync::Mutex::new(None::<NativeHostConfig>));
    let refresh = Arc::clone(&replacement);
    let (changed, changes) = tokio::sync::watch::channel(0_u64);
    let config = NativeHostConfig::new(
        format!("ws://{}/v1/companion", held.local_addr().unwrap()),
        "initial",
    )
    .with_config_refresh(changes, move || refresh.lock().unwrap().clone());
    let (stream, mut extension) = duplex(2 * MAX_NATIVE_MESSAGE_BYTES);
    let (reader, writer) = split(stream);
    let host = tokio::spawn(run_native_host(reader, writer, config));
    write_native_message(
        &mut extension,
        &json!({"kind":"pair","input":connect_request()}),
    )
    .await
    .unwrap();
    let (_stalled, _) = held.accept().await.unwrap();
    *replacement.lock().unwrap() = Some(NativeHostConfig::new(
        format!("ws://{}/v1/companion", second.local_addr()),
        second.registry().issue_pairing_code().await,
    ));
    changed.send_modify(|version| *version += 1);
    let paired = tokio::time::timeout(Duration::from_secs(2), read_native_message(&mut extension))
        .await
        .expect("an occupied non-WebSocket port must not trap native discovery")
        .unwrap()
        .unwrap();
    assert_eq!(paired["kind"], "paired");
    drop(extension);
    host.await.unwrap().unwrap();
}

/// A result the relay refuses to forward fails its one command and keeps the
/// connection: the next command on the same grant completes, and free text
/// such as `note: read this` is forwarded.
#[tokio::test]
async fn a_rejected_action_result_fails_its_command_and_keeps_the_relay() {
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
    let (host_stream, mut extension_stream) = duplex(2 * MAX_NATIVE_MESSAGE_BYTES);
    let (host_reader, host_writer) = split(host_stream);
    let host = tokio::spawn(run_native_host(host_reader, host_writer, config));
    write_native_message(
        &mut extension_stream,
        &json!({"kind": "pair", "input": connect_request}),
    )
    .await
    .unwrap();
    let paired = read_native_message(&mut extension_stream).await.unwrap();
    assert_eq!(paired.unwrap()["kind"], "paired");
    let discovery = CompanionEvent::TargetsDiscovered(TargetDiscovery {
        protocol_version: PROTOCOL_VERSION,
        profile_id: profile_id.clone(),
        targets: vec![BrowserTarget {
            target_id: "tab-1".into(),
            kind: TargetKind::Page,
        }],
    });
    write_native_message(
        &mut extension_stream,
        &serde_json::to_value(discovery).unwrap(),
    )
    .await
    .unwrap();
    server
        .wait_for_discovery(&profile_id, Duration::from_secs(1))
        .await
        .unwrap();
    let grant = server.grant_discovered_targets(&profile_id).await.unwrap();
    read_native_message(&mut extension_stream)
        .await
        .unwrap()
        .unwrap();

    for (text, rejected) in [
        ("see https://example.test/?token=private-value", true),
        ("note: read this", false),
    ] {
        let command_id = CommandId::new();
        let action = ActionRequest {
            protocol_version: PROTOCOL_VERSION,
            attachment_id: grant.attachment_id.clone(),
            command_id: command_id.clone(),
            page_id: grant.pages[0].page_id.clone(),
            operation: "a11yTree".into(),
            input: json!({}),
            deadline_unix_ms: 4_102_444_800_000,
        };
        let completed = CompanionEvent::ActionCompleted(ActionResult {
            command_id: command_id.clone(),
            interaction_path: InteractionPath::ExtensionApi,
            output: json!({"nodes": [{"role": "StaticText", "name": text}]}),
        });
        let reply = serde_json::to_value(&completed).unwrap();
        let extension = async {
            read_native_message(&mut extension_stream)
                .await
                .unwrap()
                .unwrap();
            write_native_message(&mut extension_stream, &reply)
                .await
                .unwrap();
        };
        let (result, ()) = tokio::join!(server.dispatch_action(action), extension);
        let result = result.unwrap();
        if rejected {
            let CompanionEvent::ActionFailed {
                command_id: failed,
                code,
                effect_uncertain,
                ..
            } = result
            else {
                panic!("expected actionFailed, got {result:?}");
            };
            assert_eq!((failed, code.as_str()), (command_id, "outputRejected"));
            assert!(effect_uncertain);
        } else {
            assert_eq!(result, completed);
        }
    }

    drop(extension_stream);
    host.await.unwrap().unwrap();
}

async fn within<T>(step: &str, future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {step}"))
}

/// The native host process dies mid-session. A command issued while it is
/// gone waits; the respawned host reconnects with the reconnect credential,
/// the server re-sends the same attachment grant, and the command completes
/// on the original attachment without the session being recreated.
#[tokio::test]
async fn a_respawned_native_host_restores_the_attachment_grant() {
    let server = CompanionServer::bind_loopback(CompanionServerConfig {
        bind_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        pairing_code_ttl: Duration::from_secs(60),
        attachment_ttl: Duration::from_secs(300),
    })
    .await
    .unwrap();
    let pairing_code = server.registry().issue_pairing_code().await;
    let store_dir = std::env::temp_dir().join(format!("native-host-store-{}", CommandId::new().0));
    std::fs::create_dir_all(&store_dir).unwrap();
    let store = store_dir.join("firefox-native-host-credential.json");
    let config = NativeHostConfig::new(
        format!("ws://{}/v1/companion", server.local_addr()),
        pairing_code,
    )
    .with_credential_store(store.clone());
    let connect_request = connect_request();
    let profile_id = connect_request.profile_id.clone();
    let connect = json!({"kind": "pair", "input": connect_request});
    let (host_stream, mut first_extension) = duplex(2 * MAX_NATIVE_MESSAGE_BYTES);
    let (host_reader, host_writer) = split(host_stream);
    let first_host = tokio::spawn(run_native_host(host_reader, host_writer, config.clone()));
    write_native_message(&mut first_extension, &connect)
        .await
        .unwrap();
    let paired = read_native_message(&mut first_extension).await.unwrap();
    assert_eq!(paired.unwrap()["kind"], "paired");
    let discovery = CompanionEvent::TargetsDiscovered(TargetDiscovery {
        protocol_version: PROTOCOL_VERSION,
        profile_id: profile_id.clone(),
        targets: vec![BrowserTarget {
            target_id: "tab-1".into(),
            kind: TargetKind::Page,
        }],
    });
    write_native_message(
        &mut first_extension,
        &serde_json::to_value(&discovery).unwrap(),
    )
    .await
    .unwrap();
    server
        .wait_for_discovery(&profile_id, Duration::from_secs(1))
        .await
        .unwrap();
    let grant = server.grant_discovered_targets(&profile_id).await.unwrap();
    read_native_message(&mut first_extension)
        .await
        .unwrap()
        .unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&store).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // Kill the host process: its relay and socket end without a goodbye.
    first_host.abort();
    let _ = first_host.await;
    drop(first_extension);
    // A command racing the close is routed to the dying connection and ends
    // `ConnectionClosed`; this one is issued once the server saw the close.
    within("the server to see the close", async {
        // Discovery belongs to a connection and is dropped with it.
        while server
            .wait_for_discovery(&profile_id, Duration::from_millis(10))
            .await
            .is_ok()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;

    let command_id = CommandId::new();
    let action = ActionRequest {
        protocol_version: PROTOCOL_VERSION,
        attachment_id: grant.attachment_id.clone(),
        command_id: command_id.clone(),
        page_id: grant.pages[0].page_id.clone(),
        operation: "a11yTree".into(),
        input: json!({}),
        deadline_unix_ms: 4_102_444_800_000,
    };
    let completed = CompanionEvent::ActionCompleted(ActionResult {
        command_id,
        interaction_path: InteractionPath::ExtensionApi,
        output: json!({"nodes": []}),
    });
    let reply = serde_json::to_value(&completed).unwrap();
    let respawn = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        // A new process: nothing shared with the first host but the store;
        // the descriptor is gone.
        let respawned = NativeHostConfig::from_credential_store(store.clone())
            .expect("the first host stored its reconnect credential");
        let (host_stream, mut extension) = duplex(2 * MAX_NATIVE_MESSAGE_BYTES);
        let (host_reader, host_writer) = split(host_stream);
        let host = tokio::spawn(run_native_host(host_reader, host_writer, respawned));
        write_native_message(&mut extension, &connect)
            .await
            .unwrap();
        let paired = within("paired", read_native_message(&mut extension)).await;
        assert_eq!(paired.unwrap().unwrap()["kind"], "paired");
        let regrant = within("re-sent grant", read_native_message(&mut extension)).await;
        let regrant: CompanionRequest = serde_json::from_value(regrant.unwrap().unwrap()).unwrap();
        assert_eq!(regrant, CompanionRequest::Grant(grant.clone()));
        let request = within("action", read_native_message(&mut extension)).await;
        assert_eq!(request.unwrap().unwrap()["kind"], "action");
        write_native_message(&mut extension, &reply).await.unwrap();
        (host, extension)
    };
    let (result, (host, extension)) =
        tokio::join!(within("dispatch", server.dispatch_action(action)), respawn);
    assert_eq!(result.unwrap(), completed);

    drop(extension);
    host.await.unwrap().unwrap();
}
