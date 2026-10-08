use companion_protocol::{
    is_extension_build_id, BrowserIdentity, CompanionCapabilities, CompanionEvent,
    CompanionRequest, PairRequest, EXTENSION_BUILD_HEADER, PROTOCOL_VERSION,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fmt,
    future::Future,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        client::IntoClientRequest,
        http::{header::AUTHORIZATION, HeaderValue, StatusCode},
        Error as WebSocketError, Message,
    },
};
use types::{CompanionId, ProfileId};
use url::Url;

use crate::lifecycle_log::LifecycleLog;

pub const MAX_NATIVE_MESSAGE_BYTES: usize = 1024 * 1024;
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(100);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(5);
/// The companion server pings every 30s; a socket silent for longer is dead
/// even when TCP still reports ESTABLISHED (a killed peer leaves the
/// connection half-open).
const SERVER_LIVENESS_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_SECRET_BYTES: usize = 512;

#[derive(Debug, Clone)]
pub struct NativeReconnectBackoff {
    next: Duration,
}

impl Default for NativeReconnectBackoff {
    fn default() -> Self {
        Self {
            next: INITIAL_RECONNECT_DELAY,
        }
    }
}

impl NativeReconnectBackoff {
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = self
            .next
            .checked_mul(2)
            .unwrap_or(MAX_RECONNECT_DELAY)
            .min(MAX_RECONNECT_DELAY);
        delay
    }

    pub fn reset(&mut self) {
        self.next = INITIAL_RECONNECT_DELAY;
    }
}

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
struct ReconnectCredential(String);

impl fmt::Debug for ReconnectCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ReconnectCredential([redacted])")
    }
}

#[derive(Clone)]
pub struct NativeHostConfig {
    endpoint: String,
    pairing_code: String,
    ownership_id: Option<String>,
    reconnect_credential: Arc<Mutex<Option<ReconnectCredential>>>,
    credential_store: Option<PathBuf>,
    config_refresh: Option<Arc<dyn Fn() -> Option<NativeHostConfig> + Send + Sync>>,
    config_changes: Option<watch::Receiver<u64>>,
}

/// The reconnect credential a native host persists so its successor process
/// reconnects without a freshly published descriptor. Bound to the endpoint
/// and owner it was issued for; the server stays the authority.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredCredential {
    endpoint: String,
    ownership_id: Option<String>,
    reconnect_credential: String,
}

impl NativeHostConfig {
    pub fn new(endpoint: String, pairing_code: impl Into<String>) -> Self {
        Self {
            endpoint,
            pairing_code: pairing_code.into(),
            ownership_id: None,
            reconnect_credential: Arc::new(Mutex::new(None)),
            credential_store: None,
            config_refresh: None,
            config_changes: None,
        }
    }

    /// Persist the reconnect credential at `path` (mode 0600), and reuse a
    /// stored one issued for this endpoint and owner.
    pub fn with_credential_store(self, path: PathBuf) -> Self {
        let stored = read_stored_credential(&path).filter(|stored| {
            stored.endpoint == self.endpoint && stored.ownership_id == self.ownership_id
        });
        let config = Self {
            credential_store: Some(path),
            ..self
        };
        if let Some(stored) = stored {
            if let Ok(mut credential) = config.reconnect_credential.lock() {
                *credential = Some(ReconnectCredential(stored.reconnect_credential));
            }
        }
        config
    }

    /// A config from the stored credential alone, for a host started after
    /// the descriptor was unpublished.
    pub fn from_credential_store(path: PathBuf) -> Option<Self> {
        let stored = read_stored_credential(&path)?;
        let config = Self {
            ownership_id: stored.ownership_id.clone(),
            ..Self::new(stored.endpoint.clone(), String::new())
        };
        Some(config.with_credential_store(path))
    }

    fn validate_connect(&self, request: &NativeConnectRequest) -> Result<(), NativeHostError> {
        if request.protocol_version != PROTOCOL_VERSION {
            return Err(NativeHostError::UnsupportedProtocolVersion);
        }
        validate_native_connect(request)?;
        if !self.has_reconnect_credential()? {
            validate_secret(&self.pairing_code)?;
        }
        Ok(())
    }

    fn has_pairing_code(&self) -> bool {
        validate_secret(&self.pairing_code).is_ok()
    }

    fn forget_reconnect_credential(&self) -> Result<(), NativeHostError> {
        *self
            .reconnect_credential
            .lock()
            .map_err(|_| NativeHostError::InvalidPairingMaterial)? = None;
        if let Some(path) = &self.credential_store {
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }

    /// Identify the listener's publication generation independently of its port.
    pub fn with_ownership_id(mut self, ownership_id: String) -> Self {
        self.ownership_id = (!ownership_id.is_empty()).then_some(ownership_id);
        self
    }

    /// Reload local endpoint discovery when a runtime replaces its listener.
    pub fn with_config_refresh(
        mut self,
        changes: watch::Receiver<u64>,
        refresh: impl Fn() -> Option<NativeHostConfig> + Send + Sync + 'static,
    ) -> Self {
        self.config_changes = Some(changes);
        self.config_refresh = Some(Arc::new(refresh));
        self
    }

    fn refresh_endpoint(&mut self) -> bool {
        let Some(mut next) = self.config_refresh.as_ref().and_then(|refresh| refresh()) else {
            return false;
        };
        let same_owner = next.ownership_id.is_none() || next.ownership_id == self.ownership_id;
        if (next.endpoint == self.endpoint && same_owner)
            || next
                .authentication_token()
                .and_then(|token| next.authenticated_request(&token))
                .is_err()
        {
            return false;
        }
        next.config_refresh = self.config_refresh.clone();
        next.config_changes = self.config_changes.clone();
        // A new endpoint or owner never inherits the old credential.
        if let Some(path) = self.credential_store.take() {
            let _ = std::fs::remove_file(&path);
            next.credential_store = Some(path);
        }
        *self = next;
        true
    }

    pub fn pair_request(
        &self,
        request: NativeConnectRequest,
    ) -> Result<CompanionRequest, NativeHostError> {
        if request.protocol_version != PROTOCOL_VERSION {
            return Err(NativeHostError::UnsupportedProtocolVersion);
        }
        validate_native_connect(&request)?;
        validate_secret(&self.pairing_code)?;
        Ok(CompanionRequest::Pair(PairRequest {
            protocol_version: PROTOCOL_VERSION,
            pairing_code: self.pairing_code.clone(),
            companion_id: request.companion_id,
            profile_id: request.profile_id,
            identity: request.identity,
            capabilities: request.capabilities,
        }))
    }

    fn authentication_token(&self) -> Result<String, NativeHostError> {
        if let Some(credential) = self
            .reconnect_credential
            .lock()
            .map_err(|_| NativeHostError::InvalidPairingMaterial)?
            .as_ref()
        {
            return Ok(credential.0.clone());
        }
        validate_secret(&self.pairing_code)?;
        Ok(self.pairing_code.clone())
    }

    fn has_reconnect_credential(&self) -> Result<bool, NativeHostError> {
        Ok(self
            .reconnect_credential
            .lock()
            .map_err(|_| NativeHostError::InvalidPairingMaterial)?
            .is_some())
    }

    fn store_reconnect_credential(&self, credential: String) -> Result<(), NativeHostError> {
        validate_secret(&credential)?;
        if let Some(path) = &self.credential_store {
            // Rotated on every pairing; a write failure leaves the process
            // credential in memory only.
            let _ = write_stored_credential(
                path,
                &StoredCredential {
                    endpoint: self.endpoint.clone(),
                    ownership_id: self.ownership_id.clone(),
                    reconnect_credential: credential.clone(),
                },
            );
        }
        *self
            .reconnect_credential
            .lock()
            .map_err(|_| NativeHostError::InvalidPairingMaterial)? =
            Some(ReconnectCredential(credential));
        Ok(())
    }

    fn authenticated_request(
        &self,
        token: &str,
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
        validate_secret(token)?;
        let bearer = HeaderValue::from_str(&format!("Bearer {token}"))
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
            .field("reconnect_credential", &"[redacted]")
            .finish()
    }
}

fn read_stored_credential(path: &Path) -> Option<StoredCredential> {
    let stored: StoredCredential = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    validate_secret(&stored.reconnect_credential).ok()?;
    Some(stored)
}

fn write_stored_credential(path: &Path, stored: &StoredCredential) -> std::io::Result<()> {
    let mut pending = path.as_os_str().to_owned();
    pending.push(format!(".pending-{}", std::process::id()));
    let pending = PathBuf::from(pending);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&pending)?;
        std::io::Write::write_all(&mut file, &serde_json::to_vec(stored)?)?;
        file.sync_all()?;
        std::fs::rename(&pending, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&pending);
    }
    result
}

fn validate_secret(secret: &str) -> Result<(), NativeHostError> {
    if secret.is_empty() || secret.len() > MAX_SECRET_BYTES {
        return Err(NativeHostError::InvalidPairingMaterial);
    }
    Ok(())
}

fn validate_native_connect(request: &NativeConnectRequest) -> Result<(), NativeHostError> {
    for value in [
        request.identity.browser_name.as_str(),
        request.identity.browser_version.as_str(),
        request.identity.os.as_str(),
        request.identity.profile_label.as_str(),
    ] {
        if value.is_empty() || value.len() > 256 {
            return Err(NativeHostError::InvalidProtocol);
        }
    }
    if request
        .extension_build_id
        .as_deref()
        .is_some_and(|build| !is_extension_build_id(build))
    {
        return Err(NativeHostError::InvalidProtocol);
    }
    let value = serde_json::to_value(request).map_err(|_| NativeHostError::InvalidProtocol)?;
    reject_extension_secrets(&value, 0)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeConnectRequest {
    pub protocol_version: u16,
    pub companion_id: CompanionId,
    pub profile_id: ProfileId,
    pub identity: BrowserIdentity,
    pub capabilities: CompanionCapabilities,
    /// The build the extension reports; forwarded to the runtime on connect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_build_id: Option<String>,
}

/// Secret-free enroll control message from the extension (`input` must be `{}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnrollProfileRequest {}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(
    tag = "kind",
    content = "input",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum NativeRequest {
    Pair(NativeConnectRequest),
    EnrollProfile(EnrollProfileRequest),
}

/// Operator-safe enroll failure codes surfaced to the extension (Task 6 maps these).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnrollHostError {
    ListenerUnavailable,
    BidiMissing,
    DefaultsMissing,
    Timeout,
}

impl EnrollHostError {
    pub fn code(self) -> &'static str {
        match self {
            Self::ListenerUnavailable => "listenerUnavailable",
            Self::BidiMissing => "bidiMissing",
            Self::DefaultsMissing => "defaultsMissing",
            Self::Timeout => "timeout",
        }
    }
}

/// Whether the native host should drop the enroll-time companion listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnrollFinalize {
    /// Temp companion was released; exit so day-2 serve can bind the same address.
    ReleaseListener,
    /// Paired against an already-running serve descriptor; keep the WS relay.
    KeepRelay,
}

/// CLI-supplied enroll bridge so `companion-core` does not depend on `firefox-companion`.
pub trait NativeHostEnroll: Send + Sync {
    fn enroll_and_wait_for_pair(
        &self,
        pair: NativeConnectRequest,
    ) -> impl Future<Output = Result<NativeHostConfig, EnrollHostError>> + Send;

    fn complete_enrollment(
        &self,
        pair: &NativeConnectRequest,
    ) -> impl Future<Output = Result<EnrollFinalize, EnrollHostError>> + Send;
}

/// Placeholder enroll impl for pair-only `run_native_host` callers.
pub struct NullNativeHostEnroll;

impl NativeHostEnroll for NullNativeHostEnroll {
    fn enroll_and_wait_for_pair(
        &self,
        _pair: NativeConnectRequest,
    ) -> impl Future<Output = Result<NativeHostConfig, EnrollHostError>> + Send {
        std::future::ready(Err(EnrollHostError::ListenerUnavailable))
    }

    fn complete_enrollment(
        &self,
        _pair: &NativeConnectRequest,
    ) -> impl Future<Output = Result<EnrollFinalize, EnrollHostError>> + Send {
        std::future::ready(Ok(EnrollFinalize::ReleaseListener))
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InitialPairedOutput {
    companion_id: CompanionId,
    profile_id: ProfileId,
    reconnect_credential: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    content = "output",
    rename_all = "camelCase",
    deny_unknown_fields
)]
enum InitialServerEvent {
    Paired(InitialPairedOutput),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NativeStatusOutput {
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'static str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NativeStatus {
    kind: &'static str,
    output: NativeStatusOutput,
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

fn canonical<T>(value: &Value) -> Result<Option<T>, NativeHostError>
where
    T: serde::de::DeserializeOwned + Serialize,
{
    let Ok(decoded) = serde_json::from_value::<T>(value.clone()) else {
        return Ok(None);
    };
    if serde_json::to_value(&decoded).map_err(|_| NativeHostError::InvalidProtocol)? != *value {
        return Ok(None);
    }
    Ok(Some(decoded))
}

pub fn validate_extension_message(value: Value) -> Result<Value, NativeHostError> {
    reject_extension_secrets(&value, 0)?;
    let Some(event) = canonical::<CompanionEvent>(&value)? else {
        return Err(NativeHostError::InvalidProtocol);
    };
    if matches!(event, CompanionEvent::Paired { .. }) {
        return Err(NativeHostError::InvalidProtocol);
    }
    Ok(value)
}

pub fn validate_server_message(value: Value) -> Result<Value, NativeHostError> {
    reject_extension_secrets(&value, 0)?;
    if let Some(request) = canonical::<CompanionRequest>(&value)? {
        if !matches!(request, CompanionRequest::Pair(_)) {
            return Ok(value);
        }
    }
    if let Some(event) = canonical::<CompanionEvent>(&value)? {
        if matches!(event, CompanionEvent::Paired { .. }) {
            return Ok(value);
        }
    }
    Err(NativeHostError::InvalidProtocol)
}

fn reject_extension_secrets(value: &Value, depth: usize) -> Result<(), NativeHostError> {
    reject_secrets_in_field(value, None, depth)
}

/// URL rules apply to URL-typed fields (`href`, `src`, `*url`). Any other
/// string is free text: it is checked for credentials and for the absolute
/// URLs embedded in it, never URL-parsed as a whole (`note: read this` is not
/// a URL with the scheme `note:`). The extension applies the same split; the
/// shared cases live in `tests/fixtures/extension-url-security.json`.
fn reject_secrets_in_field(
    value: &Value,
    field: Option<&str>,
    depth: usize,
) -> Result<(), NativeHostError> {
    if depth > 32 {
        return Err(NativeHostError::InvalidProtocol);
    }
    match value {
        Value::Object(object) => {
            for (name, item) in object {
                let normalized = name.to_ascii_lowercase().replace(['-', '_'], "");
                if [
                    "pairingcode",
                    "bearer",
                    "authorization",
                    "endpoint",
                    "credential",
                    "password",
                    "passwd",
                    "apikey",
                    "token",
                    "secret",
                ]
                .iter()
                .any(|marker| normalized.contains(marker))
                {
                    return Err(NativeHostError::InvalidProtocol);
                }
                reject_secrets_in_field(item, Some(name), depth + 1)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                reject_secrets_in_field(item, field, depth + 1)?;
            }
        }
        Value::String(text) if field.is_some_and(is_url_field) => reject_secret_url(text)?,
        Value::String(text) => reject_secret_text(text)?,
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

fn is_url_field(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "href" || name == "src" || name.ends_with("url")
}

fn reject_secret_text(text: &str) -> Result<(), NativeHostError> {
    if contains_explicit_credential(text) {
        return Err(NativeHostError::InvalidProtocol);
    }
    embedded_urls(text).try_for_each(reject_secret_url)
}

/// Each `scheme://…` run inside free text, without the punctuation that
/// commonly closes it in prose.
fn embedded_urls(text: &str) -> impl Iterator<Item = &str> {
    text.split_whitespace().filter_map(|token| {
        let separator = token.find("://")?;
        let start = token[..separator]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')))
            .map_or(0, |index| index + 1);
        (start < separator).then(|| {
            token[start..]
                .trim_end_matches([')', ']', '}', ',', '.', ';', ':', '!', '?', '\'', '"', '>'])
        })
    })
}

fn reject_secret_url(text: &str) -> Result<(), NativeHostError> {
    if contains_explicit_credential(text) {
        return Err(NativeHostError::InvalidProtocol);
    }
    let Ok(url) = Url::parse(text) else {
        return Ok(());
    };
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(NativeHostError::InvalidProtocol);
    }
    for (name, value) in url.query_pairs() {
        if is_sensitive_url_query_key(&name) || contains_explicit_credential(&value) {
            return Err(NativeHostError::InvalidProtocol);
        }
    }
    Ok(())
}

fn is_sensitive_url_query_key(name: &str) -> bool {
    let normalized = name.to_ascii_lowercase().replace(['-', '_'], "");
    matches!(
        normalized.as_str(),
        "authorization"
            | "bearer"
            | "token"
            | "accesstoken"
            | "refreshtoken"
            | "sessiontoken"
            | "secret"
            | "clientsecret"
            | "password"
            | "passwd"
            | "credential"
            | "apikey"
            | "accesskey"
            | "privatekey"
            | "key"
    )
}

fn contains_explicit_credential(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let has_auth_scheme = lower.char_indices().any(|(index, _)| {
        ["bearer", "basic"].iter().any(|scheme| {
            lower[index..].starts_with(scheme)
                && (index == 0
                    || lower[..index]
                        .chars()
                        .next_back()
                        .is_some_and(char::is_whitespace))
                && lower[index + scheme.len()..]
                    .chars()
                    .next()
                    .is_some_and(char::is_whitespace)
        })
    });
    has_auth_scheme
        || [
            "private-token",
            "private_token",
            "private token",
            "privatetoken",
            "private-secret",
            "private_secret",
            "private secret",
            "privatesecret",
            "private-key",
            "private_key",
            "private key",
            "privatekey",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
}

pub fn decode_native_request(value: Value) -> Result<NativeRequest, NativeHostError> {
    reject_extension_secrets(&value, 0)?;
    let request: NativeRequest =
        serde_json::from_value(value.clone()).map_err(|_| NativeHostError::InvalidProtocol)?;
    let canonical = serde_json::to_value(&request).map_err(|_| NativeHostError::InvalidProtocol)?;
    if canonical != value {
        return Err(NativeHostError::InvalidProtocol);
    }
    Ok(request)
}

fn decode_initial_paired(value: &Value) -> Result<Option<InitialPairedOutput>, NativeHostError> {
    let Some(event) = canonical::<InitialServerEvent>(value)? else {
        return Ok(None);
    };
    match event {
        InitialServerEvent::Paired(output) => {
            validate_secret(&output.reconnect_credential)?;
            Ok(Some(output))
        }
    }
}

fn public_paired(output: &InitialPairedOutput) -> Result<Value, NativeHostError> {
    serde_json::to_value(CompanionEvent::Paired {
        companion_id: output.companion_id.clone(),
        profile_id: output.profile_id.clone(),
    })
    .map_err(|_| NativeHostError::InvalidProtocol)
}

enum ConnectionResult {
    NativeClosed,
    Reconnect(&'static str),
    EndpointChanged,
}

fn connect_failure(error: &WebSocketError) -> String {
    match error {
        WebSocketError::Http(response) => format!("http {}", response.status().as_u16()),
        WebSocketError::Io(error) => format!("io {:?}", error.kind()),
        _ => "websocket handshake failed".to_owned(),
    }
}

const OUTPUT_REJECTED_CODE: &str = "outputRejected";

/// The `actionFailed` the server receives in place of an action result the
/// relay refused to forward, so the command ends with a clear error.
fn rejected_action_failure(kind: &str, command_id: Option<String>) -> Option<Value> {
    if !matches!(kind, "actionCompleted" | "actionFailed") {
        return None;
    }
    let failure = serde_json::json!({
        "kind": "actionFailed",
        "output": {
            "commandId": command_id?,
            "code": OUTPUT_REJECTED_CODE,
            "message": "the page result contains text the companion relay does not forward; \
                        the connection is intact and the same call returns the same result",
            "effectUncertain": kind == "actionCompleted",
        }
    });
    validate_extension_message(failure).ok()
}

fn message_kind(value: &Value) -> &str {
    value
        .get("kind")
        .and_then(Value::as_str)
        .filter(|kind| kind.len() <= 32 && kind.chars().all(|c| c.is_ascii_alphabetic()))
        .unwrap_or("unknown")
}

async fn wait_for_config_change(changes: &mut Option<watch::Receiver<u64>>) {
    if let Some(receiver) = changes {
        if receiver.changed().await.is_ok() {
            return;
        }
    }
    std::future::pending::<()>().await;
}

async fn sleep_or_native_closed(delay: Duration, closed: &mut watch::Receiver<bool>) -> bool {
    if *closed.borrow() {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        result = closed.changed() => result.is_err() || *closed.borrow(),
    }
}

async fn wait_for_native_close(closed: &mut watch::Receiver<bool>) {
    if *closed.borrow() {
        return;
    }
    while closed.changed().await.is_ok() {
        if *closed.borrow() {
            return;
        }
    }
}

async fn write_terminal_auth_status<W>(writer: &mut W)
where
    W: AsyncWrite + Unpin,
{
    let _ = write_native_message(
        writer,
        &serde_json::to_value(NativeStatus {
            kind: "nativeStatus",
            output: NativeStatusOutput {
                state: "invalidAuth",
                code: None,
            },
        })
        .unwrap_or(Value::Null),
    )
    .await;
}

async fn write_enroll_status<W>(
    writer: &mut W,
    state: &'static str,
    code: Option<&'static str>,
) -> Result<(), NativeHostError>
where
    W: AsyncWrite + Unpin,
{
    write_native_message(
        writer,
        &serde_json::to_value(NativeStatus {
            kind: "nativeStatus",
            output: NativeStatusOutput { state, code },
        })
        .map_err(|_| NativeHostError::InvalidProtocol)?,
    )
    .await
}

async fn write_enroll_failed<W>(
    writer: &mut W,
    error: EnrollHostError,
) -> Result<(), NativeHostError>
where
    W: AsyncWrite + Unpin,
{
    write_enroll_status(writer, "enrollFailed", Some(error.code())).await
}

async fn read_native_input<R>(
    mut native_reader: R,
    native_messages: mpsc::Sender<Result<Option<Value>, NativeHostError>>,
    native_closed: watch::Sender<bool>,
) where
    R: AsyncRead + Unpin,
{
    loop {
        let message = read_native_message(&mut native_reader).await;
        let closing = matches!(message, Ok(None) | Err(_));
        if closing {
            let _ = native_closed.send(true);
        }
        if native_messages.send(message).await.is_err() || closing {
            break;
        }
    }
}

pub async fn run_native_host<R, W>(
    native_reader: R,
    native_writer: W,
    config: NativeHostConfig,
) -> Result<(), NativeHostError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
{
    run_native_host_with_enroll(
        native_reader,
        native_writer,
        Some(config),
        None::<NullNativeHostEnroll>,
    )
    .await
}

pub async fn run_native_host_with_enroll<R, W, E>(
    native_reader: R,
    native_writer: W,
    config: Option<NativeHostConfig>,
    enroll: Option<E>,
) -> Result<(), NativeHostError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
    E: NativeHostEnroll,
{
    run_native_host_logged(
        native_reader,
        native_writer,
        config,
        enroll,
        &LifecycleLog::disabled(),
    )
    .await
}

/// [`run_native_host_with_enroll`] that records connect, disconnect, rejected
/// messages and the fatal error to `log`.
pub async fn run_native_host_logged<R, W, E>(
    native_reader: R,
    native_writer: W,
    config: Option<NativeHostConfig>,
    enroll: Option<E>,
    log: &LifecycleLog,
) -> Result<(), NativeHostError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
    E: NativeHostEnroll,
{
    let result = relay_native_host(native_reader, native_writer, config, enroll, log).await;
    match &result {
        Ok(()) => log.record("relay_closed", "extension port closed"),
        Err(error) => log.record("fatal", &error.to_string()),
    }
    result
}

async fn relay_native_host<R, W, E>(
    mut native_reader: R,
    mut native_writer: W,
    config: Option<NativeHostConfig>,
    enroll: Option<E>,
    log: &LifecycleLog,
) -> Result<(), NativeHostError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
    E: NativeHostEnroll,
{
    let first = read_native_message(&mut native_reader)
        .await?
        .ok_or(NativeHostError::MissingConnectRequest)?;
    let first = decode_native_request(first)?;

    let (connect, mut config, finalize_enroll) = match first {
        NativeRequest::Pair(input) => {
            let config = config.ok_or(NativeHostError::InvalidPairingMaterial)?;
            (input, config, false)
        }
        NativeRequest::EnrollProfile(_) => {
            let Some(enroll) = enroll.as_ref() else {
                return Err(NativeHostError::InvalidProtocol);
            };
            let pair_message = read_native_message(&mut native_reader)
                .await?
                .ok_or(NativeHostError::MissingConnectRequest)?;
            let pair_request = decode_native_request(pair_message)?;
            let NativeRequest::Pair(input) = pair_request else {
                write_enroll_failed(&mut native_writer, EnrollHostError::ListenerUnavailable)
                    .await?;
                return Ok(());
            };
            match enroll.enroll_and_wait_for_pair(input.clone()).await {
                Ok(config) => (input, config, true),
                Err(error) => {
                    write_enroll_failed(&mut native_writer, error).await?;
                    return Ok(());
                }
            }
        }
    };

    let expected_companion_id = connect.companion_id.clone();
    let expected_profile_id = connect.profile_id.clone();
    config.validate_connect(&connect)?;
    let mut config_changes = config.config_changes.clone();

    let (native_messages, mut receiver) = mpsc::channel(32);
    let (native_closed, mut native_closed_receiver) = watch::channel(false);
    let reader_task = tokio::spawn(read_native_input(
        native_reader,
        native_messages,
        native_closed,
    ));

    let result = async {
        let mut enroll_finalized = !finalize_enroll;
        let mut backoff = NativeReconnectBackoff::default();
        'reconnect: loop {
        if *native_closed_receiver.borrow() {
            break Ok(());
        }
        config.refresh_endpoint();
        let has_credential = config.has_reconnect_credential()?;
        // A host started from a stored credential has no pairing code; the
        // pair request is built only when pairing.
        let pair = if has_credential {
            String::new()
        } else {
            let pair = config.pair_request(connect.clone())?;
            serde_json::to_string(&pair).map_err(|_| NativeHostError::InvalidProtocol)?
        };
        let token = config.authentication_token()?;
        let mut request = config.authenticated_request(&token)?;
        if let Some(build) = connect.extension_build_id.as_deref() {
            let build = HeaderValue::from_str(build).map_err(|_| NativeHostError::InvalidProtocol)?;
            request.headers_mut().insert(EXTENSION_BUILD_HEADER, build);
        }
        let attempt = connect_async(request);
        tokio::pin!(attempt);
        // The server consumes a pairing code when the upgrade arrives, so an
        // attempt is abandoned only for a new endpoint or owner: a retry with
        // the same code would be refused.
        let connection = loop {
            tokio::select! {
                _ = wait_for_config_change(&mut config_changes) => {
                    if config.refresh_endpoint() {
                        backoff.reset();
                        continue 'reconnect;
                    }
                }
                _ = wait_for_native_close(&mut native_closed_receiver) => break 'reconnect Ok(()),
                result = &mut attempt => break result,
            }
        };
        let socket = match connection {
            Ok((socket, _)) => socket,
            Err(WebSocketError::Http(response))
                if response.status() == StatusCode::UNAUTHORIZED =>
            {
                // A replacement endpoint has its own credential; an old
                // listener rejecting the prior one must not block handoff.
                if config.refresh_endpoint() {
                    backoff.reset();
                    continue;
                }
                // The server is the authority: a refused stored credential is
                // deleted, and pairing falls back to the descriptor's code.
                if has_credential {
                    log.record("credential_refused", "stored reconnect credential deleted");
                    config.forget_reconnect_credential()?;
                    if config.has_pairing_code() {
                        continue;
                    }
                }
                write_terminal_auth_status(&mut native_writer).await;
                break Err(NativeHostError::InvalidPairingMaterial)
            }
            Err(error) if has_credential => {
                log.record("connect_failed", &connect_failure(&error));
                let delay = backoff.next_delay();
                if sleep_or_native_closed(delay, &mut native_closed_receiver).await {
                    break Ok(());
                }
                continue;
            }
            Err(error) => {
                log.record("connect_failed", &connect_failure(&error));
                break Err(NativeHostError::WebSocket);
            }
        };
        log.record(
            "connected",
            if has_credential { "reconnect credential" } else { "pairing code" },
        );
        backoff.reset();
        let (mut socket_writer, mut socket_reader) = socket.split();
        if !has_credential
            && socket_writer
                .send(Message::Text(pair.clone().into()))
                .await
                .is_err()
        {
            break Err(NativeHostError::WebSocket);
        }

        let liveness = tokio::time::sleep(SERVER_LIVENESS_TIMEOUT);
        tokio::pin!(liveness);
        let connection = loop {
            tokio::select! {
                _ = wait_for_config_change(&mut config_changes) => {
                    if config.refresh_endpoint() {
                        break Ok(ConnectionResult::EndpointChanged);
                    }
                }
                _ = &mut liveness => break Ok(ConnectionResult::Reconnect("server liveness timeout")),
                native = receiver.recv() => {
                    match native {
                        Some(Ok(Some(value))) => {
                            let kind = message_kind(&value).to_owned();
                            let command_id = value
                                .pointer("/output/commandId")
                                .and_then(Value::as_str)
                                .map(str::to_owned);
                            let value = match validate_extension_message(value) {
                                Ok(value) => value,
                                Err(error) => {
                                    // One unforwardable message fails its own command;
                                    // it never ends the relay and every other session.
                                    log.record("rejected_extension_message", &format!("{kind}: {error}"));
                                    match rejected_action_failure(&kind, command_id) {
                                        Some(failure) => failure,
                                        None => continue,
                                    }
                                }
                            };
                            let body = serde_json::to_string(&value)
                                .map_err(|_| NativeHostError::InvalidProtocol)?;
                            if socket_writer.send(Message::Text(body.into())).await.is_err() {
                                break Ok(ConnectionResult::Reconnect("server send failed"));
                            }
                        }
                        Some(Ok(None)) | None => break Ok(ConnectionResult::NativeClosed),
                        Some(Err(error)) => {
                            log.record("native_read_failed", &error.to_string());
                            break Err(error);
                        }
                    }
                }
                message = socket_reader.next() => {
                    liveness.as_mut().reset(tokio::time::Instant::now() + SERVER_LIVENESS_TIMEOUT);
                    match message {
                        Some(Ok(Message::Text(body))) => {
                            if body.len() > MAX_NATIVE_MESSAGE_BYTES {
                                    break Err(NativeHostError::MessageTooLarge { length: body.len() });
                            }
                            let value: Value = serde_json::from_str(body.as_str())
                                .map_err(|_| NativeHostError::InvalidJson)?;
                            let value = if let Some(initial) = decode_initial_paired(&value)? {
                                if has_credential
                                    || initial.companion_id != expected_companion_id
                                    || initial.profile_id != expected_profile_id
                                {
                                    break Err(NativeHostError::InvalidProtocol);
                                }
                                config.store_reconnect_credential(initial.reconnect_credential.clone())?;
                                public_paired(&initial)?
                            } else {
                                let kind = message_kind(&value).to_owned();
                                let value = match validate_server_message(value) {
                                    Ok(value) => value,
                                    Err(error) => {
                                        log.record("rejected_server_message", &format!("{kind}: {error}"));
                                        break Err(error);
                                    }
                                };
                                if let Ok(CompanionEvent::Paired { companion_id, profile_id }) =
                                    serde_json::from_value::<CompanionEvent>(value.clone())
                                {
                                    if companion_id != expected_companion_id || profile_id != expected_profile_id {
                                        break Err(NativeHostError::InvalidProtocol);
                                    }
                                }
                                value
                            };
                            let is_initial_pair = value
                                .get("kind")
                                .and_then(|kind| kind.as_str())
                                == Some("paired");
                            if !enroll_finalized && is_initial_pair {
                                // Persist before the extension observes durable success.
                                if let Some(enroll) = enroll.as_ref() {
                                    let finalize = match enroll.complete_enrollment(&connect).await
                                    {
                                        Ok(finalize) => finalize,
                                        Err(error) => {
                                            // Extension already has a well-formed enrollFailed;
                                            // exit the relay cleanly (not a protocol error).
                                            write_enroll_failed(&mut native_writer, error).await?;
                                            break Ok(ConnectionResult::NativeClosed);
                                        }
                                    };
                                    write_native_message(&mut native_writer, &value).await?;
                                    write_enroll_status(&mut native_writer, "enrollOk", None)
                                        .await?;
                                    enroll_finalized = true;
                                    match finalize {
                                        // Temp enrollment listener was dropped; exit so day-2
                                        // serve can bind the same address.
                                        EnrollFinalize::ReleaseListener => {
                                            break Ok(ConnectionResult::NativeClosed);
                                        }
                                        // Live serve already owns the bind; keep relaying.
                                        // Skip the fallthrough write — paired was already sent.
                                        EnrollFinalize::KeepRelay => {
                                            continue;
                                        }
                                    }
                                }
                            }
                            write_native_message(&mut native_writer, &value).await?;
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            if socket_writer.send(Message::Pong(payload)).await.is_err() {
                                break Ok(ConnectionResult::Reconnect("server pong failed"));
                            }
                        }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(Message::Close(_))) => {
                            break Ok(ConnectionResult::Reconnect("server closed"));
                        }
                        None | Some(Err(_)) => {
                            break Ok(ConnectionResult::Reconnect("server connection lost"));
                        }
                        Some(Ok(Message::Binary(_))) | Some(Ok(Message::Frame(_))) => {
                            break Err(NativeHostError::WebSocket);
                        }
                    }
                }
            }
        };

        match connection? {
            ConnectionResult::NativeClosed => break Ok(()),
            ConnectionResult::EndpointChanged => {
                log.record("disconnected", "endpoint changed");
                backoff.reset()
            }
            ConnectionResult::Reconnect(reason) if config.has_reconnect_credential()? => {
                log.record("disconnected", reason);
                let delay = backoff.next_delay();
                if sleep_or_native_closed(delay, &mut native_closed_receiver).await {
                    break Ok(());
                }
            }
            ConnectionResult::Reconnect(reason) => {
                log.record("disconnected", reason);
                break Err(NativeHostError::WebSocket);
            }
        }
        }
    }
    .await;
    reader_task.abort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_code_refresh_keeps_the_current_owner_credential() {
        let (_changed, changes) = watch::channel(0_u64);
        let mut config =
            NativeHostConfig::new("ws://127.0.0.1:9877/v1/companion".into(), "original")
                .with_ownership_id("same-owner".into())
                .with_config_refresh(changes, || {
                    Some(
                        NativeHostConfig::new(
                            "ws://127.0.0.1:9877/v1/companion".into(),
                            "rotated-code",
                        )
                        .with_ownership_id("same-owner".into()),
                    )
                });
        config
            .store_reconnect_credential("existing-credential".into())
            .unwrap();
        assert!(!config.refresh_endpoint());
        assert_eq!(
            config.authentication_token().unwrap(),
            "existing-credential"
        );
    }

    #[test]
    fn ownership_change_at_the_same_endpoint_replaces_the_old_credential() {
        let (_changed, changes) = watch::channel(0_u64);
        let mut config =
            NativeHostConfig::new("ws://127.0.0.1:9877/v1/companion".into(), "original")
                .with_ownership_id("old-owner".into())
                .with_config_refresh(changes, || {
                    Some(
                        NativeHostConfig::new(
                            "ws://127.0.0.1:9877/v1/companion".into(),
                            "new-code",
                        )
                        .with_ownership_id("new-owner".into()),
                    )
                });
        config
            .store_reconnect_credential("old-credential".into())
            .unwrap();
        assert!(config.refresh_endpoint());
        assert_eq!(config.authentication_token().unwrap(), "new-code");
        assert!(!config.has_reconnect_credential().unwrap());
    }

    #[test]
    fn endpoint_refresh_rejects_remote_and_invalid_pairing_material() {
        for (endpoint, secret) in [
            ("ws://example.com/v1/companion", "replacement"),
            ("ws://127.0.0.1:9878/v1/companion", ""),
        ] {
            let (_changed, changes) = watch::channel(0_u64);
            let mut config =
                NativeHostConfig::new("ws://127.0.0.1:9877/v1/companion".into(), "original")
                    .with_config_refresh(changes, move || {
                        Some(NativeHostConfig::new(endpoint.into(), secret))
                    });
            config
                .store_reconnect_credential("existing-credential".into())
                .unwrap();
            assert!(!config.refresh_endpoint());
            assert_eq!(config.endpoint, "ws://127.0.0.1:9877/v1/companion");
            assert_eq!(
                config.authentication_token().unwrap(),
                "existing-credential"
            );
        }
    }

    #[tokio::test]
    async fn eof_publishes_cancellation_before_a_saturated_queue_send() {
        let (messages, receiver) = mpsc::channel(1);
        messages.send(Ok(Some(Value::Null))).await.unwrap();
        let (closed, mut closed_receiver) = watch::channel(false);
        let reader = tokio::spawn(read_native_input(tokio::io::empty(), messages, closed));

        tokio::time::timeout(Duration::from_millis(50), closed_receiver.changed())
            .await
            .expect("EOF cancellation must not wait for queue capacity")
            .unwrap();
        assert!(*closed_receiver.borrow());

        drop(receiver);
        reader.await.unwrap();
    }
}
