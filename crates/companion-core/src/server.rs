use crate::{registry::PairingCodeClaim, CompanionRegistry, PairingInput};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{
        header::{AUTHORIZATION, WWW_AUTHENTICATE},
        HeaderMap, HeaderValue, StatusCode,
    },
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use companion_protocol::{CompanionEvent, CompanionRequest, PROTOCOL_VERSION};
use futures_util::{stream::SplitSink, SinkExt, StreamExt};
use serde::{
    de::{DeserializeSeed, Error as DeError, MapAccess, SeqAccess, Visitor},
    Serialize,
};
use serde_json::Value;
use std::{collections::HashSet, fmt, net::SocketAddr, sync::Arc, time::Duration};
use thiserror::Error;
use tokio::{net::TcpListener, sync::mpsc, task::JoinHandle};

const MAX_FRAME_BYTES: usize = 1024 * 1024;
const FRAME_ERROR_HEADROOM_BYTES: usize = 64 * 1024;
const OUTBOUND_QUEUE_CAPACITY: usize = 64;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct CompanionServerConfig {
    pub bind_addr: SocketAddr,
    pub pairing_code_ttl: Duration,
    pub attachment_ttl: Duration,
}

#[derive(Debug, Error)]
pub enum CompanionServerError {
    #[error("companion server address must be loopback: {0}")]
    NonLoopbackAddress(SocketAddr),
    #[error("failed to bind companion server")]
    Bind(#[source] std::io::Error),
    #[error("failed to read companion server address")]
    LocalAddress(#[source] std::io::Error),
}

#[derive(Debug)]
pub struct CompanionServer;

impl CompanionServer {
    pub async fn bind_loopback(
        config: CompanionServerConfig,
    ) -> Result<CompanionServerHandle, CompanionServerError> {
        if !config.bind_addr.ip().is_loopback() {
            return Err(CompanionServerError::NonLoopbackAddress(config.bind_addr));
        }

        let listener = TcpListener::bind(config.bind_addr)
            .await
            .map_err(CompanionServerError::Bind)?;
        let local_addr = listener
            .local_addr()
            .map_err(CompanionServerError::LocalAddress)?;
        let registry = Arc::new(CompanionRegistry::new(
            config.pairing_code_ttl,
            config.attachment_ttl,
        ));
        let state = Arc::new(ServerState {
            registry: Arc::clone(&registry),
        });
        let router = Router::new()
            .route("/v1/companion", get(companion_upgrade))
            .with_state(state);
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router).await {
                tracing::error!(%error, "companion server stopped");
            }
        });

        Ok(CompanionServerHandle {
            local_addr,
            registry,
            task,
        })
    }
}

#[derive(Debug)]
pub struct CompanionServerHandle {
    local_addr: SocketAddr,
    registry: Arc<CompanionRegistry>,
    task: JoinHandle<()>,
}

impl CompanionServerHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn registry(&self) -> Arc<CompanionRegistry> {
        Arc::clone(&self.registry)
    }
}

impl Drop for CompanionServerHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Debug)]
struct ServerState {
    registry: Arc<CompanionRegistry>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TransportErrorBody {
    code: &'static str,
    message: &'static str,
}

async fn companion_upgrade(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let Some(pairing_code) = bearer(&headers) else {
        return unauthorized_response();
    };
    let Ok(claim) = state.registry.claim_pairing_code(&pairing_code).await else {
        return unauthorized_response();
    };
    let registry = Arc::clone(&state.registry);

    upgrade
        // A small, bounded decoder headroom lets the application return the
        // typed 1 MiB limit error before closing the connection.
        .max_frame_size(MAX_FRAME_BYTES + FRAME_ERROR_HEADROOM_BYTES)
        .max_message_size(MAX_FRAME_BYTES + FRAME_ERROR_HEADROOM_BYTES)
        .on_upgrade(move |socket| serve_socket(socket, registry, claim))
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    if headers.get_all(AUTHORIZATION).iter().count() != 1 {
        return None;
    }
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?;
    if token.is_empty() || token.len() > 512 {
        return None;
    }
    Some(token.to_owned())
}

fn unauthorized_response() -> Response {
    let mut response = (
        StatusCode::UNAUTHORIZED,
        Json(TransportErrorBody {
            code: "unauthorized",
            message: "authentication required",
        }),
    )
        .into_response();
    response
        .headers_mut()
        .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}

async fn serve_socket(
    socket: WebSocket,
    registry: Arc<CompanionRegistry>,
    claim: PairingCodeClaim,
) {
    let (sink, mut stream) = socket.split();
    let (outbound, receiver) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
    let writer = tokio::spawn(write_socket(sink, receiver));

    let Ok(Some(first)) =
        tokio::time::timeout(claim.remaining(), next_request(&mut stream, &outbound)).await
    else {
        send_error_and_close(&outbound, "pairingRejected", "pairing was rejected").await;
        drop(outbound);
        let _ = writer.await;
        return;
    };
    let CompanionRequest::Pair(request) = first else {
        send_error_and_close(&outbound, "pairingRequired", "the first request must pair").await;
        drop(outbound);
        let _ = writer.await;
        return;
    };

    if request.protocol_version != PROTOCOL_VERSION || request.pairing_code != claim.pairing_code()
    {
        send_error_and_close(&outbound, "pairingRejected", "pairing was rejected").await;
        drop(outbound);
        let _ = writer.await;
        return;
    }

    let paired = registry
        .pair_claimed(
            claim,
            PairingInput {
                pairing_code: request.pairing_code,
                companion_id: request.companion_id,
                profile_id: request.profile_id,
                identity: request.identity,
                capabilities: request.capabilities,
            },
        )
        .await;
    let Ok(paired) = paired else {
        send_error_and_close(&outbound, "pairingRejected", "pairing was rejected").await;
        drop(outbound);
        let _ = writer.await;
        return;
    };
    if send_event(
        &outbound,
        &CompanionEvent::Paired {
            companion_id: paired.companion_id,
            profile_id: paired.profile_id,
        },
    )
    .await
    .is_err()
    {
        drop(outbound);
        let _ = writer.await;
        return;
    }

    while let Some(request) = next_request(&mut stream, &outbound).await {
        match request {
            CompanionRequest::Ping => {
                if send_event(&outbound, &CompanionEvent::Pong).await.is_err() {
                    break;
                }
            }
            CompanionRequest::Pair(_) | CompanionRequest::Action(_) => {
                send_error_and_close(
                    &outbound,
                    "invalidRequest",
                    "request is not valid in the current state",
                )
                .await;
                break;
            }
        }
    }

    drop(outbound);
    let _ = writer.await;
}

async fn next_request(
    stream: &mut futures_util::stream::SplitStream<WebSocket>,
    outbound: &mpsc::Sender<Message>,
) -> Option<CompanionRequest> {
    loop {
        match stream.next().await {
            Some(Ok(Message::Text(text))) => {
                if text.len() > MAX_FRAME_BYTES {
                    send_error_and_close(
                        outbound,
                        "frameTooLarge",
                        "frame exceeds the 1 MiB limit",
                    )
                    .await;
                    return None;
                }
                match strict_decode(text.as_str()) {
                    Ok(request) => return Some(request),
                    Err(()) => {
                        send_error_and_close(
                            outbound,
                            "invalidRequest",
                            "request must be strict companion JSON",
                        )
                        .await;
                        return None;
                    }
                }
            }
            Some(Ok(Message::Ping(payload))) => {
                if outbound.send(Message::Pong(payload)).await.is_err() {
                    return None;
                }
            }
            Some(Ok(Message::Pong(_))) => {}
            Some(Ok(Message::Close(_))) | None => return None,
            Some(Ok(Message::Binary(_))) => {
                send_error_and_close(
                    outbound,
                    "invalidRequest",
                    "request must be strict companion JSON",
                )
                .await;
                return None;
            }
            Some(Err(_)) => {
                send_error_and_close(outbound, "frameTooLarge", "frame exceeds the 1 MiB limit")
                    .await;
                return None;
            }
        }
    }
}

fn strict_decode(text: &str) -> Result<CompanionRequest, ()> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    DuplicateKeyRejector
        .deserialize(&mut deserializer)
        .map_err(|_| ())?;
    deserializer.end().map_err(|_| ())?;

    let value: Value = serde_json::from_str(text).map_err(|_| ())?;
    let request: CompanionRequest = serde_json::from_value(value.clone()).map_err(|_| ())?;
    let canonical = serde_json::to_value(&request).map_err(|_| ())?;
    if value != canonical {
        return Err(());
    }
    Ok(request)
}

struct DuplicateKeyRejector;

impl<'de> DeserializeSeed<'de> for DuplicateKeyRejector {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for DuplicateKeyRejector {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON without duplicate object keys")
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence.next_element_seed(DuplicateKeyRejector)?.is_some() {}
        Ok(())
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = HashSet::new();
        while let Some(key) = object.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(A::Error::custom("duplicate object key"));
            }
            object.next_value_seed(DuplicateKeyRejector)?;
        }
        Ok(())
    }
}

async fn send_event(outbound: &mpsc::Sender<Message>, event: &CompanionEvent) -> Result<(), ()> {
    let body = serde_json::to_string(event).map_err(|_| ())?;
    outbound
        .send(Message::Text(body.into()))
        .await
        .map_err(|_| ())
}

async fn send_error_and_close(
    outbound: &mpsc::Sender<Message>,
    code: &'static str,
    message: &'static str,
) {
    let body = TransportErrorBody { code, message };
    if let Ok(body) = serde_json::to_string(&body) {
        let _ = outbound.send(Message::Text(body.into())).await;
    }
    let _ = outbound.send(Message::Close(None)).await;
}

async fn write_socket(
    mut sink: SplitSink<WebSocket, Message>,
    mut receiver: mpsc::Receiver<Message>,
) {
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.tick().await;
    loop {
        tokio::select! {
            message = receiver.recv() => {
                let Some(message) = message else {
                    break;
                };
                let closing = matches!(message, Message::Close(_));
                if sink.send(message).await.is_err() || closing {
                    break;
                }
            }
            _ = heartbeat.tick() => {
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }
}
