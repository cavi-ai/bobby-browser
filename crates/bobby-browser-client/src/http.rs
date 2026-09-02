use chrono::{Duration as ChronoDuration, Utc};
use reqwest::{Client, Method};
use serde::de::DeserializeOwned;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::time::Duration;
use thiserror::Error;
use uuid::Uuid;

use crate::{
    CheckpointRequest, CommandEnvelope, CommandOutcome, CreateSessionRequest, FormSnapshot,
    OpenPageRequest, PageId, PageState, RecoveryDecision, RecoveryStatus, RuntimeInfo, SessionId,
    SessionState, WorkflowCheckpoint, WorkflowId, CURRENT_INTERFACE_VERSION,
};

/// Client hard bounds for [`BrowserRuntimeClient::artifact`], mirroring the
/// TypeScript SDK defaults.
pub const DEFAULT_MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_SDK_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;

/// Errors returned by [`BrowserRuntimeClient`].
///
/// Messages are redacted so the bearer token never appears in transport,
/// HTTP, or protocol error text.
#[derive(Debug, Error)]
pub enum ClientError {
    /// Network or HTTP-client failure before a valid response body.
    #[error("transport error: {0}")]
    Transport(String),
    /// Non-success HTTP status with response body text.
    #[error("HTTP {status}: {message}")]
    Http { status: u16, message: String },
    /// Response body could not be interpreted, or client preconditions failed.
    #[error("protocol error: {0}")]
    Protocol(String),
}

impl ClientError {
    fn redact(self, bearer: &str) -> Self {
        match self {
            Self::Transport(message) => Self::Transport(message.replace(bearer, "")),
            Self::Http { status, message } => Self::Http {
                status,
                message: message.replace(bearer, ""),
            },
            Self::Protocol(message) => Self::Protocol(message.replace(bearer, "")),
        }
    }
}

/// Options for a single request.
#[derive(Debug, Clone, Default)]
pub struct RequestOptions {
    /// Request timeout (defaults to 30 seconds).
    pub timeout: Option<Duration>,
    /// Value for `x-correlation-id` (generated UUID when omitted).
    pub correlation_id: Option<String>,
    /// Value for `idempotency-key` when the operation supports it.
    pub idempotency_key: Option<String>,
}

/// Authenticated HTTP client for the Bobby Browser `/v1` runtime interface.
///
/// Construct with [`BrowserRuntimeClient::new`]. `base_url` is the runtime
/// origin; a trailing slash and a trailing `/v1` are stripped.
#[derive(Debug, Clone)]
pub struct BrowserRuntimeClient {
    base_url: String,
    bearer_token: String,
    http: Client,
    default_timeout: Duration,
    max_artifact_bytes: u64,
}

impl BrowserRuntimeClient {
    /// Create a client.
    ///
    /// `base_url` is the runtime origin (`http://127.0.0.1:7777` or `…/v1`).
    /// Returns [`ClientError::Protocol`] when `base_url` or `bearer_token` is empty.
    pub fn new(
        base_url: impl Into<String>,
        bearer_token: impl Into<String>,
    ) -> Result<Self, ClientError> {
        Self::with_options(base_url, bearer_token, DEFAULT_MAX_ARTIFACT_BYTES)
    }

    /// Create a client with an explicit artifact byte cap (at most
    /// [`MAX_SDK_ARTIFACT_BYTES`]).
    pub fn with_options(
        base_url: impl Into<String>,
        bearer_token: impl Into<String>,
        max_artifact_bytes: u64,
    ) -> Result<Self, ClientError> {
        let bearer_token = bearer_token.into();
        if bearer_token.is_empty() {
            return Err(ClientError::Protocol(
                "bearerToken must not be empty".into(),
            ));
        }
        if max_artifact_bytes == 0 || max_artifact_bytes > MAX_SDK_ARTIFACT_BYTES {
            return Err(ClientError::Protocol(
                "maxArtifactBytes must be positive and within the SDK allocation cap".into(),
            ));
        }
        let base_url = normalize_base_url(base_url.into());
        if base_url.is_empty() {
            return Err(ClientError::Protocol("baseUrl must not be empty".into()));
        }
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| ClientError::Transport(error.to_string()))?;
        Ok(Self {
            base_url,
            bearer_token,
            http,
            default_timeout: Duration::from_secs(30),
            max_artifact_bytes,
        })
    }

    /// `GET /v1/runtime` — version, capabilities, and load counters.
    pub async fn runtime_info(
        &self,
        options: Option<RequestOptions>,
    ) -> Result<RuntimeInfo, ClientError> {
        self.json(Method::GET, "/v1/runtime", None::<()>, options)
            .await
    }

    /// `POST /v1/sessions` — create a browser session.
    pub async fn create_session(
        &self,
        input: &CreateSessionRequest,
        options: Option<RequestOptions>,
    ) -> Result<SessionState, ClientError> {
        self.json(Method::POST, "/v1/sessions", Some(input), options)
            .await
    }

    /// `GET /v1/sessions` — list active sessions.
    pub async fn list_sessions(
        &self,
        options: Option<RequestOptions>,
    ) -> Result<Vec<SessionState>, ClientError> {
        self.json(Method::GET, "/v1/sessions", None::<()>, options)
            .await
    }

    /// `DELETE /v1/sessions/{id}` — tear down a session (`204` on success).
    pub async fn delete_session(
        &self,
        session_id: &SessionId,
        options: Option<RequestOptions>,
    ) -> Result<(), ClientError> {
        self.empty(
            Method::DELETE,
            &format!("/v1/sessions/{}", session_id.0),
            options,
        )
        .await
    }

    /// `POST /v1/pages` — open a page in a session.
    pub async fn open_page(
        &self,
        input: &OpenPageRequest,
        options: Option<RequestOptions>,
    ) -> Result<PageState, ClientError> {
        self.json(Method::POST, "/v1/pages", Some(input), options)
            .await
    }

    /// `POST /v1/commands` — submit a command envelope (primitive or intent).
    pub async fn submit(
        &self,
        input: &CommandEnvelope,
        options: Option<RequestOptions>,
    ) -> Result<CommandOutcome, ClientError> {
        self.json(Method::POST, "/v1/commands", Some(input), options)
            .await
    }

    async fn empty(
        &self,
        method: Method,
        path: &str,
        options: Option<RequestOptions>,
    ) -> Result<(), ClientError> {
        let options = options.unwrap_or_default();
        let timeout = options.timeout.unwrap_or(self.default_timeout);
        let deadline =
            Utc::now() + ChronoDuration::from_std(timeout).unwrap_or(ChronoDuration::seconds(30));
        let correlation = options
            .correlation_id
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base_url))
            .timeout(timeout)
            .header("authorization", format!("Bearer {}", self.bearer_token))
            .header("x-interface-version", CURRENT_INTERFACE_VERSION)
            .header("x-correlation-id", correlation)
            .header("x-deadline", deadline.to_rfc3339());
        if let Some(key) = options.idempotency_key {
            request = request.header("idempotency-key", key);
        }

        let response = request.send().await.map_err(|error| {
            ClientError::Transport(error.to_string()).redact(&self.bearer_token)
        })?;
        let status = response.status();
        if status == reqwest::StatusCode::NO_CONTENT || status.is_success() {
            return Ok(());
        }
        let text = response.text().await.unwrap_or_default();
        Err(ClientError::Http {
            status: status.as_u16(),
            message: text,
        }
        .redact(&self.bearer_token))
    }

    async fn json<B, T>(
        &self,
        method: Method,
        path: &str,
        body: Option<B>,
        options: Option<RequestOptions>,
    ) -> Result<T, ClientError>
    where
        B: Serialize,
        T: DeserializeOwned,
    {
        let options = options.unwrap_or_default();
        let timeout = options.timeout.unwrap_or(self.default_timeout);
        let deadline =
            Utc::now() + ChronoDuration::from_std(timeout).unwrap_or(ChronoDuration::seconds(30));
        let correlation = options
            .correlation_id
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base_url))
            .timeout(timeout)
            .header("authorization", format!("Bearer {}", self.bearer_token))
            .header("x-interface-version", CURRENT_INTERFACE_VERSION)
            .header("x-correlation-id", correlation)
            .header("x-deadline", deadline.to_rfc3339());
        if let Some(key) = options.idempotency_key {
            request = request.header("idempotency-key", key);
        }
        if let Some(body) = body {
            request = request
                .header("content-type", "application/json")
                .json(&body);
        }

        let response = request.send().await.map_err(|error| {
            ClientError::Transport(error.to_string()).redact(&self.bearer_token)
        })?;
        let status = response.status();
        let text = response.text().await.map_err(|error| {
            ClientError::Transport(error.to_string()).redact(&self.bearer_token)
        })?;
        if !status.is_success() {
            return Err(ClientError::Http {
                status: status.as_u16(),
                message: text,
            }
            .redact(&self.bearer_token));
        }
        serde_json::from_str(&text).map_err(|error| {
            ClientError::Protocol(format!("invalid JSON body: {error}")).redact(&self.bearer_token)
        })
    }

    /// Raw-bytes GET for binary endpoints (`GET /v1/artifacts/{id}`): no JSON
    /// decoding, the response body is the artifact content. Returns the body
    /// plus the response's media-type essence and `Content-Length` for callers
    /// that verify the body against a reference.
    async fn bytes_with_headers(
        &self,
        path: &str,
        options: Option<RequestOptions>,
    ) -> Result<(Vec<u8>, (String, Option<u64>)), ClientError> {
        let options = options.unwrap_or_default();
        let timeout = options.timeout.unwrap_or(self.default_timeout);
        let deadline =
            Utc::now() + ChronoDuration::from_std(timeout).unwrap_or(ChronoDuration::seconds(30));
        let correlation = options
            .correlation_id
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let response = self
            .http
            .get(format!("{}{path}", self.base_url))
            .timeout(timeout)
            .header("authorization", format!("Bearer {}", self.bearer_token))
            .header("x-interface-version", CURRENT_INTERFACE_VERSION)
            .header("x-correlation-id", correlation)
            .header("x-deadline", deadline.to_rfc3339())
            .send()
            .await
            .map_err(|error| {
                ClientError::Transport(error.to_string()).redact(&self.bearer_token)
            })?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(ClientError::Http {
                status: status.as_u16(),
                message: text,
            }
            .redact(&self.bearer_token));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
            })
            .unwrap_or_default();
        let content_length = response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        let body = response.bytes().await.map_err(|error| {
            ClientError::Transport(error.to_string()).redact(&self.bearer_token)
        })?;
        Ok((body.to_vec(), (content_type, content_length)))
    }

    /// `GET /v1/sessions/{session}/pages/{page}/forms` — semantic form snapshot.
    ///
    /// `max_controls` bounds the inventory (1..=512), mirroring the
    /// TypeScript `formSnapshot(sessionId, pageId, { maxControls })`.
    pub async fn form_snapshot(
        &self,
        session_id: &SessionId,
        page_id: &PageId,
        max_controls: Option<u32>,
        options: Option<RequestOptions>,
    ) -> Result<FormSnapshot, ClientError> {
        if let Some(bound) = max_controls {
            if !(1..=512).contains(&bound) {
                return Err(ClientError::Protocol(
                    "maxControls must be between 1 and 512".into(),
                ));
            }
        }
        let query = max_controls
            .map(|bound| format!("?maxControls={bound}"))
            .unwrap_or_default();
        self.json(
            Method::GET,
            &format!(
                "/v1/sessions/{}/pages/{}/forms{query}",
                session_id.0, page_id.0
            ),
            None::<()>,
            options,
        )
        .await
    }

    /// `POST /v1/checkpoints` — persist a workflow checkpoint.
    pub async fn checkpoint(
        &self,
        input: &CheckpointRequest,
        options: Option<RequestOptions>,
    ) -> Result<WorkflowCheckpoint, ClientError> {
        self.json(Method::POST, "/v1/checkpoints", Some(input), options)
            .await
    }

    /// `GET /v1/recovery/{workflow}` — current recovery status for a workflow.
    pub async fn recovery_status(
        &self,
        workflow_id: &WorkflowId,
        options: Option<RequestOptions>,
    ) -> Result<RecoveryStatus, ClientError> {
        self.json(
            Method::GET,
            &format!("/v1/recovery/{}", workflow_id.0),
            None::<()>,
            options,
        )
        .await
    }

    /// `POST /v1/recovery/{workflow}` — resume, reconcile, or restart a
    /// workflow. A `needsReconciliation` decision answers HTTP `409`, which
    /// this method surfaces as `ClientError::Http` carrying the decision body.
    pub async fn recover(
        &self,
        workflow_id: &WorkflowId,
        options: Option<RequestOptions>,
    ) -> Result<RecoveryDecision, ClientError> {
        self.json(
            Method::POST,
            &format!("/v1/recovery/{}", workflow_id.0),
            None::<()>,
            options,
        )
        .await
    }

    /// `GET /v1/artifacts/{id}` — fetch an artifact and verify it against its
    /// reference before any bytes reach the caller.
    ///
    /// Checks the reference bounds (artifact id shape, lowercase SHA-256
    /// digest, byte cap), then the response `Content-Type` essence and
    /// `Content-Length`, then the SHA-256 digest of the buffered body.
    pub async fn artifact(
        &self,
        reference: &ArtifactReference,
        options: Option<RequestOptions>,
    ) -> Result<Vec<u8>, ClientError> {
        validate_artifact_reference(reference, self.max_artifact_bytes)?;
        let (bytes, (content_type, content_length)) = self
            .bytes_with_headers(&format!("/v1/artifacts/{}", reference.artifact_id), options)
            .await?;
        if content_type != reference.media_type_essence() {
            return Err(ClientError::Protocol(
                "artifact media type does not match its reference".into(),
            ));
        }
        if content_length.is_none_or(|length| length != reference.bytes)
            || bytes.len() as u64 != reference.bytes
        {
            return Err(ClientError::Protocol(
                "artifact content length does not match its reference".into(),
            ));
        }
        let digest = hex::encode(Sha256::digest(&bytes));
        if digest != reference.sha256 {
            return Err(ClientError::Protocol("artifact verification failed".into()));
        }
        Ok(bytes)
    }
}

/// Typed description of an artifact as issued by the runtime (evidence
/// `artifactId`/`mediaType`/`bytes`/`sha256` fields). The client verifies a
/// fetched body against it before returning any bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactReference {
    pub artifact_id: String,
    pub sha256: String,
    pub bytes: u64,
    pub media_type: String,
}

impl ArtifactReference {
    /// Lowercase media-type essence (`text/plain` from `text/plain; charset=utf-8`).
    ///
    /// Returns the empty string when the media type has no valid essence —
    /// [`validate_artifact_reference`] rejects such references before fetch.
    pub fn media_type_essence(&self) -> String {
        media_type_essence(&self.media_type).unwrap_or_default()
    }
}

fn validate_artifact_reference(
    reference: &ArtifactReference,
    max_artifact_bytes: u64,
) -> Result<(), ClientError> {
    let artifact_id_ok = !reference.artifact_id.is_empty()
        && reference.artifact_id.len() <= 128
        && reference
            .artifact_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-');
    let digest_ok = reference.sha256.len() == 64
        && reference
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
    let media_type_ok = media_type_essence(&reference.media_type).is_some();
    if !artifact_id_ok || !digest_ok || !media_type_ok || reference.bytes > max_artifact_bytes {
        return Err(ClientError::Protocol(
            "artifact reference is outside the client hard bound".into(),
        ));
    }
    Ok(())
}

/// Lowercased media-type essence, `None` when the value has no valid
/// `type/subtype` form (RFC 9110 token characters only).
fn media_type_essence(value: &str) -> Option<String> {
    const TOKEN_EXTRA: &[u8] = b"!#$%&'*+.^_`|~-";
    let essence = value.split(';').next()?.trim();
    let (type_part, subtype_part) = essence.split_once('/')?;
    let valid = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || TOKEN_EXTRA.contains(&byte))
    };
    if !valid(type_part) || !valid(subtype_part) {
        return None;
    }
    Some(essence.to_ascii_lowercase())
}

fn normalize_base_url(value: String) -> String {
    let trimmed = value.trim_end_matches('/').to_string();
    if let Some(stripped) = trimmed.strip_suffix("/v1") {
        stripped.trim_end_matches('/').to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AttemptId, CheckpointId, CommandId};
    use axum::http::HeaderMap;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::Router;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::net::TcpListener;

    async fn runtime_handler(headers: HeaderMap) -> impl IntoResponse {
        assert!(headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == "Bearer test-token"));
        assert_eq!(
            headers
                .get("x-interface-version")
                .and_then(|v| v.to_str().ok()),
            Some(CURRENT_INTERFACE_VERSION)
        );
        assert!(headers.get("x-correlation-id").is_some());
        assert!(headers.get("x-deadline").is_some());
        (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            json!({
                "version": env!("CARGO_PKG_VERSION"),
                "capabilities": ["session:read"],
                "active_sessions": 0,
                "queued_jobs": 0,
                "uptime_ms": 1,
            })
            .to_string(),
        )
    }

    #[tokio::test]
    async fn runtime_info_sends_required_headers() {
        let app = Router::new().route("/v1/runtime", get(runtime_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = BrowserRuntimeClient::new(format!("http://{addr}/v1"), "test-token").unwrap();
        let info = client.runtime_info(None).await.unwrap();
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(info.active_sessions, 0);
    }

    #[tokio::test]
    async fn normalize_strips_v1_suffix() {
        assert_eq!(
            normalize_base_url("http://127.0.0.1:7777/v1/".into()),
            "http://127.0.0.1:7777"
        );
    }

    #[tokio::test]
    async fn rejects_empty_bearer() {
        let err = BrowserRuntimeClient::new("http://127.0.0.1:7777", "").unwrap_err();
        assert!(matches!(err, ClientError::Protocol(_)));
    }

    #[tokio::test]
    async fn rejects_out_of_cap_max_artifact_bytes() {
        let err = BrowserRuntimeClient::with_options(
            "http://127.0.0.1:7777",
            "test-token",
            MAX_SDK_ARTIFACT_BYTES + 1,
        )
        .unwrap_err();
        assert!(matches!(err, ClientError::Protocol(_)));
        let err = BrowserRuntimeClient::with_options("http://127.0.0.1:7777", "test-token", 0)
            .unwrap_err();
        assert!(matches!(err, ClientError::Protocol(_)));
    }

    async fn spawn(app: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    async fn capture_uri(
        path: &str,
        response: impl IntoResponse + Clone + Send + Sync + 'static,
    ) -> (String, tokio::sync::mpsc::Receiver<String>) {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let response = Arc::new(response);
        let app = Router::new().route(
            path,
            axum::routing::any(move |uri: axum::http::Uri| {
                let response = Arc::clone(&response);
                let tx = tx.clone();
                async move {
                    let _ = tx.send(uri.to_string()).await;
                    (*response).clone()
                }
            }),
        );
        let base = spawn(app).await;
        (base, rx)
    }

    #[tokio::test]
    async fn form_snapshot_builds_bounded_query() {
        let (base, mut rx) = capture_uri(
            "/v1/sessions/{session}/pages/{page}/forms",
            axum::Json(json!({
                "schemaVersion": 1,
                "pageId": "00000000-0000-4000-8000-000000000009",
                "forms": [],
                "unownedControls": [],
                "truncated": false,
            })),
        )
        .await;
        let client = BrowserRuntimeClient::new(base, "test-token").unwrap();
        let session = SessionId::new();
        let page = PageId::new();
        let snapshot = client
            .form_snapshot(&session, &page, Some(7), None)
            .await
            .unwrap();
        assert!(!snapshot.truncated);
        assert_eq!(
            rx.recv().await.unwrap(),
            format!(
                "/v1/sessions/{}/pages/{}/forms?maxControls=7",
                session.0, page.0
            )
        );
        assert!(client
            .form_snapshot(&session, &page, Some(0), None)
            .await
            .is_err());
        assert!(client
            .form_snapshot(&session, &page, Some(513), None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn checkpoint_posts_the_request_body() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let app = Router::new().route(
            "/v1/checkpoints",
            axum::routing::post(move |body: String| {
                let tx = tx.clone();
                async move {
                    let _ = tx.send(body).await;
                    (
                        StatusCode::OK,
                        axum::Json(json!({
                            "schemaVersion": 1,
                            "checkpointId": CheckpointId::new(),
                            "workflowId": WorkflowId::new(),
                            "attemptId": AttemptId::new(),
                            "sessionId": SessionId::new(),
                            "pageId": PageId::new(),
                            "restartUrl": "https://example.test",
                            "currentUrl": "https://example.test",
                            "recoveryClass": "replayable",
                            "invariants": [],
                            "replayableInputs": [],
                            "evidence": [],
                            "createdAt": "2026-09-02T00:00:00Z",
                        })),
                    )
                }
            }),
        );
        let base = spawn(app).await;
        let client = BrowserRuntimeClient::new(base, "test-token").unwrap();
        let request = CheckpointRequest {
            checkpoint: serde_json::from_value(json!({
                "schemaVersion": 1,
                "checkpointId": CheckpointId::new(),
                "workflowId": WorkflowId::new(),
                "attemptId": AttemptId::new(),
                "sessionId": SessionId::new(),
                "pageId": PageId::new(),
                "restartUrl": "https://example.test",
                "currentUrl": "https://example.test",
                "recoveryClass": "replayable",
                "invariants": [],
                "replayableInputs": [],
                "evidence": [],
                "createdAt": "2026-09-02T00:00:00Z",
            }))
            .unwrap(),
            evidence_refs: vec![CommandId::new()],
        };
        let checkpoint = client.checkpoint(&request, None).await.unwrap();
        assert_eq!(
            checkpoint.schema_version,
            WorkflowCheckpoint::SCHEMA_VERSION
        );
        let sent: serde_json::Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(sent["evidenceRefs"].as_array().map(Vec::len), Some(1));
    }

    #[tokio::test]
    async fn recovery_status_gets_the_workflow_path() {
        let workflow = WorkflowId::new();
        let (base, mut rx) = capture_uri(
            "/v1/recovery/{workflow}",
            axum::Json(json!({
                "workflowId": workflow.0,
                "checkpoint": null,
                "receipts": [],
            })),
        )
        .await;
        let client = BrowserRuntimeClient::new(base, "test-token").unwrap();
        // `checkpoint: null` violates the wire contract, so the request fails
        // client-side decoding — the point of this test is the URI shape.
        assert!(client.recovery_status(&workflow, None).await.is_err());
        assert_eq!(
            rx.recv().await.unwrap(),
            format!("/v1/recovery/{}", workflow.0)
        );
    }

    #[tokio::test]
    async fn recover_posts_the_workflow_path() {
        let workflow = WorkflowId::new();
        let (base, mut rx) = capture_uri(
            "/v1/recovery/{workflow}",
            axum::Json(json!({
                "status": "resumed",
                "checkpointId": CheckpointId::new(),
                "attemptId": AttemptId::new(),
                "evidence": [],
            })),
        )
        .await;
        let client = BrowserRuntimeClient::new(base, "test-token").unwrap();
        let decision = client.recover(&workflow, None).await.unwrap();
        match decision {
            RecoveryDecision::Resumed { .. } => {}
            other => panic!("unexpected decision: {other:?}"),
        }
        assert_eq!(
            rx.recv().await.unwrap(),
            format!("/v1/recovery/{}", workflow.0)
        );
    }

    #[tokio::test]
    async fn recover_surfaces_needs_reconciliation_409_body() {
        let workflow = WorkflowId::new();
        let (base, mut rx) = capture_uri(
            "/v1/recovery/{workflow}",
            (
                StatusCode::CONFLICT,
                axum::Json(json!({
                    "status": "needsReconciliation",
                    "checkpointId": CheckpointId::new(),
                    "attemptId": AttemptId::new(),
                    "reason": "diverged",
                    "evidence": [],
                })),
            ),
        )
        .await;
        let client = BrowserRuntimeClient::new(base, "test-token").unwrap();
        let err = client.recover(&workflow, None).await.unwrap_err();
        assert!(matches!(err, ClientError::Http { status: 409, .. }));
        assert_eq!(
            rx.recv().await.unwrap(),
            format!("/v1/recovery/{}", workflow.0)
        );
    }

    const ARTIFACT_BODY: &[u8] = b"artifact";
    // sha256("artifact")
    const ARTIFACT_DIGEST: &str =
        "c7c5c1d70c5dec4416ab6158afd0b223ef40c29b1dc1f97ed9428b94d4cadb1c";

    fn artifact_reference(sha256: &str, bytes: u64, media_type: &str) -> ArtifactReference {
        ArtifactReference {
            artifact_id: CommandId::new().0.to_string(),
            sha256: sha256.into(),
            bytes,
            media_type: media_type.into(),
        }
    }

    async fn spawn_artifact_server(
        media_type: &'static str,
        body: &'static [u8],
        extra_headers: bool,
    ) -> String {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            media_type.parse().unwrap(),
        );
        if extra_headers {
            headers.insert(
                axum::http::header::CONTENT_LENGTH,
                body.len().to_string().parse().unwrap(),
            );
        }
        let app = Router::new().route(
            "/v1/artifacts/{id}",
            get(move || {
                let headers = headers.clone();
                async move {
                    if extra_headers {
                        axum::response::IntoResponse::into_response((StatusCode::OK, headers, body))
                    } else {
                        // Chunked transfer: no Content-Length reaches the client.
                        let chunks = vec![Ok::<Vec<u8>, std::io::Error>(body.to_vec())];
                        axum::response::IntoResponse::into_response((
                            StatusCode::OK,
                            headers,
                            axum::body::Body::from_stream(futures_util::stream::iter(chunks)),
                        ))
                    }
                }
            }),
        );
        spawn(app).await
    }

    #[tokio::test]
    async fn artifact_verifies_digest_length_and_media_type() {
        let base = spawn_artifact_server("application/octet-stream", ARTIFACT_BODY, true).await;
        let client = BrowserRuntimeClient::new(base, "test-token").unwrap();
        let reference = artifact_reference(
            ARTIFACT_DIGEST,
            ARTIFACT_BODY.len() as u64,
            "application/octet-stream",
        );
        let body = client.artifact(&reference, None).await.unwrap();
        assert_eq!(body, ARTIFACT_BODY);

        let bad_digest = artifact_reference(
            &"00".repeat(32),
            ARTIFACT_BODY.len() as u64,
            "application/octet-stream",
        );
        let err = client.artifact(&bad_digest, None).await.unwrap_err();
        assert!(
            matches!(err, ClientError::Protocol(message) if message.contains("verification failed"))
        );

        let bad_media =
            artifact_reference(ARTIFACT_DIGEST, ARTIFACT_BODY.len() as u64, "text/plain");
        let err = client.artifact(&bad_media, None).await.unwrap_err();
        assert!(matches!(err, ClientError::Protocol(message) if message.contains("media type")));

        let bad_length = artifact_reference(
            ARTIFACT_DIGEST,
            ARTIFACT_BODY.len() as u64 + 1,
            "application/octet-stream",
        );
        let err = client.artifact(&bad_length, None).await.unwrap_err();
        assert!(
            matches!(err, ClientError::Protocol(message) if message.contains("content length"))
        );

        // A mixed-case media type still parses to the lowercase essence, so
        // validation passes — but it must never equal a differing server
        // type. A truly invalid media type (no type/subtype) is rejected
        // before any request is issued.
        let mixed_case = ArtifactReference {
            media_type: "Application/Octet-Stream".into(),
            ..artifact_reference(
                ARTIFACT_DIGEST,
                ARTIFACT_BODY.len() as u64,
                "application/octet-stream",
            )
        };
        assert_eq!(mixed_case.media_type_essence(), "application/octet-stream");
        let empty_media = artifact_reference(ARTIFACT_DIGEST, ARTIFACT_BODY.len() as u64, "");
        assert!(matches!(
            validate_artifact_reference(&empty_media, DEFAULT_MAX_ARTIFACT_BYTES),
            Err(ClientError::Protocol(message)) if message.contains("hard bound")
        ));
        let garbage_media = artifact_reference(
            ARTIFACT_DIGEST,
            ARTIFACT_BODY.len() as u64,
            "not a media type",
        );
        assert!(matches!(
            validate_artifact_reference(&garbage_media, DEFAULT_MAX_ARTIFACT_BYTES),
            Err(ClientError::Protocol(message)) if message.contains("hard bound")
        ));
        // An explicit lowercase essence with parameters matches fine.
        let with_params = ArtifactReference {
            media_type: "application/octet-stream; charset=binary".into(),
            ..artifact_reference(
                ARTIFACT_DIGEST,
                ARTIFACT_BODY.len() as u64,
                "application/octet-stream",
            )
        };
        assert_eq!(with_params.media_type_essence(), "application/octet-stream");
    }

    #[tokio::test]
    async fn artifact_rejects_missing_content_length() {
        let base = spawn_artifact_server("application/octet-stream", ARTIFACT_BODY, false).await;
        let client = BrowserRuntimeClient::new(base, "test-token").unwrap();
        let reference = artifact_reference(
            ARTIFACT_DIGEST,
            ARTIFACT_BODY.len() as u64,
            "application/octet-stream",
        );
        let err = client.artifact(&reference, None).await.unwrap_err();
        assert!(
            matches!(err, ClientError::Protocol(message) if message.contains("content length"))
        );
    }

    #[tokio::test]
    async fn artifact_reference_rejects_out_of_cap_before_any_request() {
        let client =
            BrowserRuntimeClient::with_options("http://127.0.0.1:7777", "test-token", 1).unwrap();
        let reference = artifact_reference(ARTIFACT_DIGEST, 2, "application/octet-stream");
        let err = client.artifact(&reference, None).await.unwrap_err();
        assert!(matches!(err, ClientError::Protocol(_)));
    }

    #[tokio::test]
    async fn artifact_reference_rejects_bad_shapes_before_any_request() {
        for reference in [
            artifact_reference(ARTIFACT_DIGEST, 8, ""),
            artifact_reference("ZZ", 8, "application/octet-stream"),
            artifact_reference("AB".repeat(32).as_str(), 8, "application/octet-stream"),
        ] {
            assert!(matches!(
                validate_artifact_reference(&reference, DEFAULT_MAX_ARTIFACT_BYTES),
                Err(ClientError::Protocol(_))
            ));
        }
        let upper = artifact_reference(&"AB".repeat(32), 8, "application/octet-stream");
        assert!(matches!(
            validate_artifact_reference(&upper, DEFAULT_MAX_ARTIFACT_BYTES),
            Err(ClientError::Protocol(_))
        ));
    }
}
