use companion_core::{
    encode_native_message, read_native_message, run_native_host, validate_protocol_message,
    write_native_message, CompanionServer, CompanionServerConfig, NativeConnectRequest,
    NativeHostConfig, NativeHostError, MAX_NATIVE_MESSAGE_BYTES,
};
use companion_protocol::{
    BrowserEngine, BrowserIdentity, CompanionCapabilities, CompanionRequest, PROTOCOL_VERSION,
};
use serde_json::json;
use std::{
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{duplex, split, AsyncRead, ReadBuf};
use types::{CompanionId, ProfileId};

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
fn native_host_forwards_only_canonical_protocol_messages() {
    assert!(validate_protocol_message(json!({"kind": "pong"})).is_ok());
    assert!(validate_protocol_message(json!({"kind": "unknown"})).is_err());
    assert!(validate_protocol_message(json!({"kind": "pong", "extra": true})).is_err());
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

    write_native_message(&mut extension_stream, &json!({"kind": "ping"}))
        .await
        .unwrap();
    assert_eq!(
        read_native_message(&mut extension_stream)
            .await
            .unwrap()
            .unwrap(),
        json!({"kind": "pong"})
    );

    drop(extension_stream);
    host.await.unwrap().unwrap();
}
