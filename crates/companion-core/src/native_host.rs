use companion_protocol::{
    BrowserIdentity, CompanionCapabilities, CompanionEvent, CompanionRequest, PairRequest,
    PROTOCOL_VERSION,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fmt, net::IpAddr};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        client::IntoClientRequest,
        http::{header::AUTHORIZATION, HeaderValue},
        Message,
    },
};
use types::{CompanionId, ProfileId};

pub const MAX_NATIVE_MESSAGE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Error)]
pub enum NativeHostError {
    #[error("native messaging I/O failed")]
    Io(#[from] std::io::Error),
    #[error("native message exceeds the 1 MiB limit: {length} bytes")]
    MessageTooLarge { length: usize },
    #[error("native message is not valid JSON")]
    InvalidJson,
    #[error("native message is not canonical protocol v1 JSON")]
    InvalidProtocol,
    #[error("native connect request uses an unsupported protocol version")]
    UnsupportedProtocolVersion,
    #[error("native companion endpoint is invalid")]
    InvalidEndpoint,
    #[error("native companion endpoint must be loopback")]
    NonLoopbackEndpoint,
    #[error("native companion pairing material is invalid")]
    InvalidPairingMaterial,
    #[error("native companion WebSocket failed")]
    WebSocket,
    #[error("native companion closed before pairing")]
    MissingConnectRequest,
}

#[derive(Clone)]
pub struct NativeHostConfig {
    endpoint: String,
    pairing_code: String,
}

impl NativeHostConfig {
    pub fn new(endpoint: String, pairing_code: impl Into<String>) -> Self {
        Self {
            endpoint,
            pairing_code: pairing_code.into(),
        }
    }

    pub fn pair_request(
        &self,
        request: NativeConnectRequest,
    ) -> Result<CompanionRequest, NativeHostError> {
        if request.protocol_version != PROTOCOL_VERSION {
            return Err(NativeHostError::UnsupportedProtocolVersion);
        }
        if self.pairing_code.is_empty() || self.pairing_code.len() > 512 {
            return Err(NativeHostError::InvalidPairingMaterial);
        }
        Ok(CompanionRequest::Pair(PairRequest {
            protocol_version: PROTOCOL_VERSION,
            pairing_code: self.pairing_code.clone(),
            companion_id: request.companion_id,
            profile_id: request.profile_id,
            identity: request.identity,
            capabilities: request.capabilities,
        }))
    }

    fn authenticated_request(
        &self,
    ) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, NativeHostError> {
        let mut request = self
            .endpoint
            .as_str()
            .into_client_request()
            .map_err(|_| NativeHostError::InvalidEndpoint)?;
        let host = request
            .uri()
            .host()
            .ok_or(NativeHostError::InvalidEndpoint)?;
        let loopback = host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<IpAddr>()
                .map(|address| address.is_loopback())
                .unwrap_or(false);
        if !loopback {
            return Err(NativeHostError::NonLoopbackEndpoint);
        }
        if self.pairing_code.is_empty() || self.pairing_code.len() > 512 {
            return Err(NativeHostError::InvalidPairingMaterial);
        }
        let bearer = HeaderValue::from_str(&format!("Bearer {}", self.pairing_code))
            .map_err(|_| NativeHostError::InvalidPairingMaterial)?;
        request.headers_mut().insert(AUTHORIZATION, bearer);
        Ok(request)
    }
}

impl fmt::Debug for NativeHostConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeHostConfig")
            .field("endpoint", &self.endpoint)
            .field("pairing_code", &"[redacted]")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeConnectRequest {
    pub protocol_version: u16,
    pub companion_id: CompanionId,
    pub profile_id: ProfileId,
    pub identity: BrowserIdentity,
    pub capabilities: CompanionCapabilities,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    content = "input",
    rename_all = "camelCase",
    deny_unknown_fields
)]
enum NativeRequest {
    Pair(NativeConnectRequest),
}

pub fn encode_native_message(value: &Value) -> Result<Vec<u8>, NativeHostError> {
    let payload = serde_json::to_vec(value).map_err(|_| NativeHostError::InvalidJson)?;
    if payload.len() > MAX_NATIVE_MESSAGE_BYTES {
        return Err(NativeHostError::MessageTooLarge {
            length: payload.len(),
        });
    }
    let length = u32::try_from(payload.len()).map_err(|_| NativeHostError::MessageTooLarge {
        length: payload.len(),
    })?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&length.to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub async fn read_native_message<R>(reader: &mut R) -> Result<Option<Value>, NativeHostError>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0_u8; 4];
    let first = reader.read(&mut header[..1]).await?;
    if first == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut header[1..]).await?;
    let length = u32::from_le_bytes(header) as usize;
    if length > MAX_NATIVE_MESSAGE_BYTES {
        return Err(NativeHostError::MessageTooLarge { length });
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload).await?;
    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(|_| NativeHostError::InvalidJson)
}

pub async fn write_native_message<W>(writer: &mut W, value: &Value) -> Result<(), NativeHostError>
where
    W: AsyncWrite + Unpin,
{
    let frame = encode_native_message(value)?;
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

pub fn validate_protocol_message(value: Value) -> Result<Value, NativeHostError> {
    if let Ok(request) = serde_json::from_value::<CompanionRequest>(value.clone()) {
        if serde_json::to_value(request).map_err(|_| NativeHostError::InvalidProtocol)? == value {
            return Ok(value);
        }
    }
    if let Ok(event) = serde_json::from_value::<CompanionEvent>(value.clone()) {
        if serde_json::to_value(event).map_err(|_| NativeHostError::InvalidProtocol)? == value {
            return Ok(value);
        }
    }
    Err(NativeHostError::InvalidProtocol)
}

fn decode_native_connect(value: Value) -> Result<NativeConnectRequest, NativeHostError> {
    let request: NativeRequest =
        serde_json::from_value(value.clone()).map_err(|_| NativeHostError::InvalidProtocol)?;
    let canonical = serde_json::to_value(&request).map_err(|_| NativeHostError::InvalidProtocol)?;
    if canonical != value {
        return Err(NativeHostError::InvalidProtocol);
    }
    match request {
        NativeRequest::Pair(input) => Ok(input),
    }
}

pub async fn run_native_host<R, W>(
    mut native_reader: R,
    mut native_writer: W,
    config: NativeHostConfig,
) -> Result<(), NativeHostError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
{
    let connect = read_native_message(&mut native_reader)
        .await?
        .ok_or(NativeHostError::MissingConnectRequest)?;
    let pair = config.pair_request(decode_native_connect(connect)?)?;
    let request = config.authenticated_request()?;
    let (socket, _) = connect_async(request)
        .await
        .map_err(|_| NativeHostError::WebSocket)?;
    let (mut socket_writer, mut socket_reader) = socket.split();
    let pair = serde_json::to_string(&pair).map_err(|_| NativeHostError::InvalidProtocol)?;
    socket_writer
        .send(Message::Text(pair.into()))
        .await
        .map_err(|_| NativeHostError::WebSocket)?;

    let (native_messages, mut receiver) = mpsc::channel(32);
    let reader_task = tokio::spawn(async move {
        loop {
            let message = read_native_message(&mut native_reader).await;
            let closing = matches!(message, Ok(None) | Err(_));
            if native_messages.send(message).await.is_err() || closing {
                break;
            }
        }
    });

    let result = loop {
        tokio::select! {
            native = receiver.recv() => {
                match native {
                    Some(Ok(Some(value))) => {
                        let value = validate_protocol_message(value)?;
                        let body = serde_json::to_string(&value).map_err(|_| NativeHostError::InvalidProtocol)?;
                        socket_writer.send(Message::Text(body.into())).await.map_err(|_| NativeHostError::WebSocket)?;
                    }
                    Some(Ok(None)) | None => break Ok(()),
                    Some(Err(error)) => break Err(error),
                }
            }
            message = socket_reader.next() => {
                match message {
                    Some(Ok(Message::Text(body))) => {
                        if body.len() > MAX_NATIVE_MESSAGE_BYTES {
                            break Err(NativeHostError::MessageTooLarge { length: body.len() });
                        }
                        let value: Value = serde_json::from_str(body.as_str()).map_err(|_| NativeHostError::InvalidJson)?;
                        let value = validate_protocol_message(value)?;
                        write_native_message(&mut native_writer, &value).await?;
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        socket_writer.send(Message::Pong(payload)).await.map_err(|_| NativeHostError::WebSocket)?;
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => break Ok(()),
                    Some(Ok(Message::Binary(_))) | Some(Ok(Message::Frame(_))) | Some(Err(_)) => {
                        break Err(NativeHostError::WebSocket);
                    }
                }
            }
        }
    };
    reader_task.abort();
    result
}
