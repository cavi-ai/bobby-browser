use jiff::SignedDuration;
use reqwest::{Client, Method};
use serde::de::DeserializeOwned;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::time::Duration;
use thiserror::Error;
use uuid::Uuid;

use crate::{
    CheckpointRequest, CommandEnvelope, CommandOutcome, ContextAskResponse, ContextMissReason,
    ContextNeighborsResponse, ContextNextStep, ContextSiteResponse, CreateSessionRequest,
    FormSnapshot, JobId, JobResolutionReceipt, JobResolutionRequest, JobStatusResponse,
    JobSubmitResponse, OpenPageRequest, PageId, PageState, RecoveryDecision, RecoveryStatus,
    RuntimeInfo, SessionId, SessionState, SubmitJobRequest, WorkflowCheckpoint, WorkflowId,
    CURRENT_INTERFACE_VERSION,
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
#[derive(Clone)]
pub struct BrowserRuntimeClient {
    base_url: String,
    bearer_token: String,
    http: Client,
    default_timeout: Duration,
    max_artifact_bytes: u64,
}

impl std::fmt::Debug for BrowserRuntimeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserRuntimeClient")
            .field("default_timeout", &self.default_timeout)
            .field("max_artifact_bytes", &self.max_artifact_bytes)
            .finish_non_exhaustive()
    }
}

const MAX_JSON_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;

async fn bounded_body(mut response: reqwest::Response, limit: u64) -> Result<Vec<u8>, ClientError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        return Err(ClientError::Protocol(
            "response exceeds the client byte limit".into(),
        ));
    }
    let mut result = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| ClientError::Transport(e.without_url().to_string()))?
    {
        if chunk.len() as u64 > limit.saturating_sub(result.len() as u64) {
            return Err(ClientError::Protocol(
                "response exceeds the client byte limit".into(),
            ));
        }
        result.reserve_exact(chunk.len());
        result.extend_from_slice(&chunk);
    }
    Ok(result)
}

fn command_status(outcome: &CommandOutcome) -> reqwest::StatusCode {
    use reqwest::StatusCode as S;
    match outcome {
        CommandOutcome::Completed { .. } | CommandOutcome::Restarted { .. } => S::OK,
        CommandOutcome::RetryableFailure { .. } => S::SERVICE_UNAVAILABLE,
        CommandOutcome::NeedsReconciliation { .. } => S::CONFLICT,
        CommandOutcome::PolicyDenied { .. } => S::FORBIDDEN,
        CommandOutcome::ResourceExhausted { .. } => S::TOO_MANY_REQUESTS,
        CommandOutcome::Failed { error, .. } if error.code == crate::ErrorCode::InvalidRequest => {
            S::UNPROCESSABLE_ENTITY
        }
        CommandOutcome::Failed { error, .. } if error.code == crate::ErrorCode::NotFound => {
            S::NOT_FOUND
        }
        CommandOutcome::Failed { .. } => S::INTERNAL_SERVER_ERROR,
    }
}

fn recovery_status_code(outcome: &RecoveryDecision) -> reqwest::StatusCode {
    match outcome {
        RecoveryDecision::NeedsReconciliation { .. } => reqwest::StatusCode::CONFLICT,
        _ => reqwest::StatusCode::OK,
    }
}

fn request_deadline(timeout: Duration) -> Result<crate::WireTimestamp, ClientError> {
    let duration = SignedDuration::try_from(timeout).unwrap_or(SignedDuration::from_secs(30));
    crate::WireTimestamp::now()
        .checked_add(duration)
        .ok_or_else(|| {
            ClientError::Protocol("request timeout exceeds supported deadline range".into())
        })
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
            .map_err(|error| ClientError::Transport(error.without_url().to_string()))?;
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

    /// `GET /v1/runtime?engine=...` — guarantees for loaded candidates of one engine.
    pub async fn runtime_info_for_engine(
        &self,
        engine: crate::SkillBrowserEngine,
        options: Option<RequestOptions>,
    ) -> Result<RuntimeInfo, ClientError> {
        let engine = match engine {
            crate::SkillBrowserEngine::Chromium => "chromium",
            crate::SkillBrowserEngine::Firefox => "firefox",
            crate::SkillBrowserEngine::WebKit => "webKit",
        };
        self.json(
            Method::GET,
            &format!("/v1/runtime?engine={engine}"),
            None::<()>,
            options,
        )
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

    /// `GET /v1/sessions/{id}/capabilities` — passive availability observations.
    pub async fn session_capabilities(
        &self,
        session_id: &SessionId,
        options: Option<RequestOptions>,
    ) -> Result<crate::SessionCapabilities, ClientError> {
        let report: crate::SessionCapabilities = self
            .json(
                Method::GET,
                &format!("/v1/sessions/{}/capabilities", session_id.0),
                None::<()>,
                options,
            )
            .await?;
        if report.schema_version != crate::SessionCapabilities::SCHEMA_VERSION
            || report.session_id != *session_id
            || report.features.is_empty()
            || report.features.len() > 64
            || report
                .features
                .iter()
                .map(|feature| &feature.name)
                .collect::<std::collections::HashSet<_>>()
                .len()
                != report.features.len()
        {
            return Err(ClientError::Protocol(
                "invalid session capability response".into(),
            ));
        }
        Ok(report)
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
        self.json_mapped(
            Method::POST,
            "/v1/commands",
            Some(input),
            options,
            None,
            Some(command_status),
        )
        .await
    }

    /// `GET /v1/context/ask` — resolve one described control from retained context.
    pub async fn context_ask(
        &self,
        session_id: &SessionId,
        page_id: &PageId,
        description: &str,
        options: Option<RequestOptions>,
    ) -> Result<ContextAskResponse, ClientError> {
        let path = context_query_path("ask", session_id, page_id, description)?;
        let response: ContextAskResponse =
            self.json(Method::GET, &path, None::<()>, options).await?;
        if !valid_context_ask_response(&response) {
            return Err(ClientError::Protocol(
                "context ask response violates the hit/miss contract".into(),
            ));
        }
        Ok(response)
    }

    /// `GET /v1/context/neighbors` — return the remembered form around one control.
    pub async fn context_neighbors(
        &self,
        session_id: &SessionId,
        page_id: &PageId,
        description: &str,
        options: Option<RequestOptions>,
    ) -> Result<ContextNeighborsResponse, ClientError> {
        let path = context_query_path("neighbors", session_id, page_id, description)?;
        let response: ContextNeighborsResponse =
            self.json(Method::GET, &path, None::<()>, options).await?;
        if !valid_context_neighbors_response(&response) {
            return Err(ClientError::Protocol(
                "context neighbors response violates the hit/miss contract".into(),
            ));
        }
        Ok(response)
    }

    /// `GET /v1/context/site/{key}` — return retained structure for one site key.
    pub async fn context_site(
        &self,
        site_key: &str,
        options: Option<RequestOptions>,
    ) -> Result<ContextSiteResponse, ClientError> {
        if site_key.is_empty() {
            return Err(ClientError::Protocol("site key must not be empty".into()));
        }
        let mut encoded =
            url::Url::parse("http://context.invalid/").expect("static context URL must parse");
        encoded
            .path_segments_mut()
            .expect("hierarchical context URL has path segments")
            .clear()
            .push(site_key);
        self.json(
            Method::GET,
            &format!(
                "/v1/context/site/{}",
                encoded.path().trim_start_matches('/')
            ),
            None::<()>,
            options,
        )
        .await
    }

    /// `POST /v1/jobs` — submit a bounded runtime job.
    pub async fn submit_job(
        &self,
        input: &SubmitJobRequest,
        options: Option<RequestOptions>,
    ) -> Result<JobSubmitResponse, ClientError> {
        input
            .validate()
            .map_err(|message| ClientError::Protocol(message.into()))?;
        self.json_with_status(
            Method::POST,
            "/v1/jobs",
            Some(input),
            options,
            Some(reqwest::StatusCode::CREATED),
        )
        .await
    }

    /// `GET /v1/jobs/{job_id}` — read the authenticated principal's job.
    pub async fn job_status(
        &self,
        job_id: &JobId,
        options: Option<RequestOptions>,
    ) -> Result<JobStatusResponse, ClientError> {
        let response: JobStatusResponse = self
            .json(
                Method::GET,
                &format!("/v1/jobs/{job_id}"),
                None::<()>,
                options,
            )
            .await?;
        response
            .validate()
            .map_err(|message| ClientError::Protocol(message.into()))?;
        Ok(response)
    }

    /// Record an owner-scoped operator attestation. This never replays the job.
    pub async fn resolve_job(
        &self,
        job_id: &JobId,
        input: &JobResolutionRequest,
        options: Option<RequestOptions>,
    ) -> Result<JobResolutionReceipt, ClientError> {
        input
            .validate()
            .map_err(|message| ClientError::Protocol(message.into()))?;
        let receipt: JobResolutionReceipt = self
            .json_with_status(
                Method::POST,
                &format!("/v1/jobs/{job_id}/resolution"),
                Some(input),
                options,
                Some(reqwest::StatusCode::OK),
            )
            .await?;
        receipt
            .validate()
            .map_err(|message| ClientError::Protocol(message.into()))?;
        if receipt.job_id != job_id.as_str()
            || receipt.decision != input.decision
            || receipt.evidence_sha256 != input.evidence_sha256
        {
            return Err(ClientError::Protocol(
                "resolution receipt does not match its request".into(),
            ));
        }
        Ok(receipt)
    }

    /// `DELETE /v1/jobs/{job_id}` — cancel the authenticated principal's job.
    pub async fn cancel_job(
        &self,
        job_id: &JobId,
        options: Option<RequestOptions>,
    ) -> Result<(), ClientError> {
        self.empty_with_status(
            Method::DELETE,
            &format!("/v1/jobs/{job_id}"),
            options,
            Some(reqwest::StatusCode::NO_CONTENT),
        )
        .await
    }

    async fn empty(
        &self,
        method: Method,
        path: &str,
        options: Option<RequestOptions>,
    ) -> Result<(), ClientError> {
        self.empty_with_status(method, path, options, None).await
    }

    async fn empty_with_status(
        &self,
        method: Method,
        path: &str,
        options: Option<RequestOptions>,
        expected_status: Option<reqwest::StatusCode>,
    ) -> Result<(), ClientError> {
        let options = options.unwrap_or_default();
        let timeout = options.timeout.unwrap_or(self.default_timeout);
        let deadline = request_deadline(timeout)?;
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
            .header("x-deadline", deadline.rfc3339());
        if let Some(key) = options.idempotency_key {
            request = request.header("idempotency-key", key);
        }

        let response = request.send().await.map_err(|error| {
            ClientError::Transport(error.without_url().to_string()).redact(&self.bearer_token)
        })?;
        let status = response.status();
        if expected_status.map_or_else(|| status.is_success(), |expected| status == expected) {
            return Ok(());
        }
        let text = String::from_utf8_lossy(
            &bounded_body(response, MAX_JSON_RESPONSE_BYTES)
                .await
                .map_err(|e| e.redact(&self.bearer_token))?,
        )
        .into_owned();
        if status.is_success() {
            return Err(ClientError::Protocol(format!(
                "unexpected HTTP status {}",
                status.as_u16()
            )));
        }
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
        self.json_with_status(method, path, body, options, None)
            .await
    }

    async fn json_with_status<B, T>(
        &self,
        method: Method,
        path: &str,
        body: Option<B>,
        options: Option<RequestOptions>,
        expected_status: Option<reqwest::StatusCode>,
    ) -> Result<T, ClientError>
    where
        B: Serialize,
        T: DeserializeOwned,
    {
        self.json_mapped(method, path, body, options, expected_status, None)
            .await
    }

    async fn json_mapped<B, T>(
        &self,
        method: Method,
        path: &str,
        body: Option<B>,
        options: Option<RequestOptions>,
        expected_status: Option<reqwest::StatusCode>,
        status_for: Option<fn(&T) -> reqwest::StatusCode>,
    ) -> Result<T, ClientError>
    where
        B: Serialize,
        T: DeserializeOwned,
    {
        let options = options.unwrap_or_default();
        let timeout = options.timeout.unwrap_or(self.default_timeout);
        let deadline = request_deadline(timeout)?;
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
            .header("x-deadline", deadline.rfc3339());
        if let Some(key) = options.idempotency_key {
            request = request.header("idempotency-key", key);
        }
        if let Some(body) = body {
            request = request
                .header("content-type", "application/json")
                .json(&body);
        }

        let response = request.send().await.map_err(|error| {
            ClientError::Transport(error.without_url().to_string()).redact(&self.bearer_token)
        })?;
        let status = response.status();
        let bytes = bounded_body(response, MAX_JSON_RESPONSE_BYTES)
            .await
            .map_err(|e| e.redact(&self.bearer_token))?;
        let parsed: Result<T, _> = serde_json::from_slice(&bytes);
        if let (Some(status_for), Ok(value)) = (status_for, parsed.as_ref()) {
            if status_for(value) != status {
                return Err(ClientError::Protocol(format!(
                    "unexpected HTTP status {} for outcome",
                    status.as_u16()
                )));
            }
        } else if !status.is_success() {
            return Err(ClientError::Http {
                status: status.as_u16(),
                message: String::from_utf8_lossy(&bytes).into_owned(),
            }
            .redact(&self.bearer_token));
        }
        if expected_status.is_some_and(|expected| status != expected) {
            return Err(ClientError::Protocol(format!(
                "unexpected HTTP status {}",
                status.as_u16()
            )));
        }
        parsed.map_err(|error| {
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
        reference: &ArtifactReference,
        options: Option<RequestOptions>,
    ) -> Result<(Vec<u8>, (String, Option<u64>)), ClientError> {
        let options = options.unwrap_or_default();
        let timeout = options.timeout.unwrap_or(self.default_timeout);
        let deadline = request_deadline(timeout)?;
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
            .header("x-deadline", deadline.rfc3339())
            .send()
            .await
            .map_err(|error| {
                ClientError::Transport(error.without_url().to_string()).redact(&self.bearer_token)
            })?;
        let status = response.status();
        if !status.is_success() {
            let text = String::from_utf8_lossy(
                &bounded_body(response, MAX_JSON_RESPONSE_BYTES)
                    .await
                    .map_err(|e| e.redact(&self.bearer_token))?,
            )
            .into_owned();
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
        if content_type != reference.media_type_essence() {
            return Err(ClientError::Protocol(
                "artifact media type does not match its reference".into(),
            ));
        }
        if content_length != Some(reference.bytes) {
            return Err(ClientError::Protocol(
                "artifact content length does not match its reference".into(),
            ));
        }
        let body = bounded_body(response, reference.bytes)
            .await
            .map_err(|e| e.redact(&self.bearer_token))?;
        Ok((body, (content_type, content_length)))
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
    /// workflow. A `needsReconciliation` decision is returned from HTTP `409`.
    pub async fn recover(
        &self,
        workflow_id: &WorkflowId,
        options: Option<RequestOptions>,
    ) -> Result<RecoveryDecision, ClientError> {
        self.json_mapped(
            Method::POST,
            &format!("/v1/recovery/{}", workflow_id.0),
            None::<()>,
            options,
            None,
            Some(recovery_status_code),
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
            .bytes_with_headers(
                &format!("/v1/artifacts/{}", reference.artifact_id),
                reference,
                options,
            )
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

fn context_query_path(
    kind: &str,
    session_id: &SessionId,
    page_id: &PageId,
    description: &str,
) -> Result<String, ClientError> {
    if !(1..=256).contains(&description.len()) {
        return Err(ClientError::Protocol(
            "description must contain between 1 and 256 bytes".into(),
        ));
    }
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("sessionId", &session_id.0.to_string());
    query.append_pair("pageId", &page_id.0.to_string());
    query.append_pair("description", description);
    Ok(format!("/v1/context/{kind}?{}", query.finish()))
}

fn valid_context_ask_response(response: &ContextAskResponse) -> bool {
    if response.hit {
        response.answer.is_some() && response.reason.is_none() && response.next_step.is_none()
    } else {
        response.answer.is_none()
            && response.reason == Some(ContextMissReason::NotRemembered)
            && response.next_step == Some(ContextNextStep::A11ySnapshot)
    }
}

fn valid_context_neighbors_response(response: &ContextNeighborsResponse) -> bool {
    if response.hit {
        response.neighbors.is_some() && response.reason.is_none() && response.next_step.is_none()
    } else {
        response.neighbors.is_none()
            && response.reason == Some(ContextMissReason::NotRemembered)
            && response.next_step == Some(ContextNextStep::A11ySnapshot)
    }
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
    #[test]
    fn oversized_request_deadline_returns_error() {
        assert!(matches!(
            request_deadline(Duration::from_secs(400_000_000_000)),
            Err(ClientError::Protocol(_))
        ));
    }

    use crate::{
        AttemptId, CheckpointId, CommandId, JobId, JobPriority, JobStatus, SubmitJobRequest,
    };
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
    async fn runtime_info_for_engine_sends_scope() {
        async fn scoped(uri: axum::http::Uri) -> impl IntoResponse {
            assert_eq!(uri.query(), Some("engine=firefox"));
            axum::Json(
                json!({"engineScope":"firefox","version":"test","capabilities":[],
                "active_sessions":0,"queued_jobs":0,"uptime_ms":1}),
            )
        }
        let app = Router::new().route("/v1/runtime", get(scoped));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = BrowserRuntimeClient::new(format!("http://{addr}"), "test-token").unwrap();
        let info = client
            .runtime_info_for_engine(crate::SkillBrowserEngine::Firefox, None)
            .await
            .unwrap();
        assert_eq!(info.engine_scope, Some(crate::SkillBrowserEngine::Firefox));
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
    async fn context_queries_encode_inputs_and_validate_responses() {
        let answer = json!({
            "target": { "role": "button", "accessibleName": "Continue" },
            "confidence": 0.9,
            "observedAt": { "kind": "persisted" },
            "source": "observed",
        });
        let (base, mut ask_uri) = capture_uri(
            "/v1/context/ask",
            axum::Json(json!({ "answer": answer, "hit": true })),
        )
        .await;
        let client = BrowserRuntimeClient::new(base, "test-token").unwrap();
        let session = SessionId::new();
        let page = PageId::new();
        let response = client
            .context_ask(&session, &page, "Continue & review", None)
            .await
            .unwrap();
        assert!(response.hit);
        assert_eq!(response.answer.unwrap().target.accessible_name, "Continue");
        let uri = ask_uri.recv().await.unwrap();
        let query = uri.split_once('?').unwrap().1;
        let pairs = url::form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(pairs.get("sessionId"), Some(&session.0.to_string()));
        assert_eq!(pairs.get("pageId"), Some(&page.0.to_string()));
        assert_eq!(
            pairs.get("description").map(String::as_str),
            Some("Continue & review")
        );

        let (base, mut neighbors_uri) = capture_uri(
            "/v1/context/neighbors",
            axum::Json(json!({
                "neighbors": null,
                "hit": false,
                "reason": "notRemembered",
                "nextStep": "a11y_snapshot"
            })),
        )
        .await;
        let client = BrowserRuntimeClient::new(base, "test-token").unwrap();
        let response = client
            .context_neighbors(&session, &page, "Continue", None)
            .await
            .unwrap();
        assert!(!response.hit);
        assert!(response.neighbors.is_none());
        assert!(neighbors_uri
            .recv()
            .await
            .unwrap()
            .starts_with("/v1/context/neighbors?"));

        let (base, _) = capture_uri(
            "/v1/context/neighbors",
            axum::Json(json!({ "neighbors": null, "hit": true })),
        )
        .await;
        let client = BrowserRuntimeClient::new(base, "test-token").unwrap();
        assert!(matches!(
            client
                .context_neighbors(&session, &page, "Continue", None)
                .await,
            Err(ClientError::Protocol(_))
        ));
        assert!(matches!(
            client.context_ask(&session, &page, "", None).await,
            Err(ClientError::Protocol(_))
        ));
        assert!(matches!(
            client
                .context_ask(&session, &page, &"x".repeat(257), None)
                .await,
            Err(ClientError::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn context_site_encodes_one_path_segment() {
        let (base, mut uri) = capture_uri(
            "/v1/context/site/{key}",
            axum::Json(json!({ "site": null })),
        )
        .await;
        let client = BrowserRuntimeClient::new(base, "test-token").unwrap();
        let response = client
            .context_site("https://example.test/account settings", None)
            .await
            .unwrap();
        assert!(response.site.is_none());
        assert_eq!(
            uri.recv().await.unwrap(),
            "/v1/context/site/https:%2F%2Fexample.test%2Faccount%20settings"
        );
        assert!(matches!(
            client.context_site("", None).await,
            Err(ClientError::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn submit_job_posts_the_public_wire_shape() {
        let job_id = JobId::new();
        let response_job_id = job_id.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let app = Router::new().route(
            "/v1/jobs",
            axum::routing::post(move |body: String| {
                let tx = tx.clone();
                let response_job_id = response_job_id.clone();
                async move {
                    let _ = tx.send(body).await;
                    (
                        StatusCode::CREATED,
                        axum::Json(json!({
                            "jobId": response_job_id,
                            "status": "pending",
                        })),
                    )
                }
            }),
        );
        let client = BrowserRuntimeClient::new(spawn(app).await, "test-token").unwrap();
        let response = client
            .submit_job(
                &SubmitJobRequest {
                    name: "fetch-report".into(),
                    payload: Some(json!({ "reportId": "weekly" })),
                    priority: Some(JobPriority::High),
                    max_retries: Some(2),
                    timeout_ms: Some(5_000),
                },
                None,
            )
            .await
            .unwrap();

        assert_eq!(response.job_id, job_id);
        assert_eq!(response.status, JobStatus::Pending);
        let body: serde_json::Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(
            body,
            json!({
                "name": "fetch-report",
                "payload": { "reportId": "weekly" },
                "priority": "high",
                "maxRetries": 2,
                "timeoutMs": 5_000,
            })
        );
    }

    #[tokio::test]
    async fn resolution_client_rejects_unbound_or_unverified_receipts() {
        let id = JobId::new();
        let input = JobResolutionRequest {
            decision: crate::JobResolutionDecision::EffectAbsent,
            evidence_sha256: "a".repeat(64),
        };
        for (job_id, provenance, expected_ok) in [
            (id.clone(), "operatorAttested", true),
            (JobId::new(), "operatorAttested", false),
            (id.clone(), "runtimeVerified", false),
        ] {
            let (base, mut rx) = capture_uri("/v1/jobs/{job}/resolution", axum::Json(json!({"jobId":job_id,"decision":"effectAbsent","evidenceSha256":"a".repeat(64),"actor":Uuid::new_v4(),"resolvedAt":crate::WireTimestamp::now(),"provenance":provenance}))).await;
            let client = BrowserRuntimeClient::new(base, "test-token").unwrap();
            assert_eq!(
                client.resolve_job(&id, &input, None).await.is_ok(),
                expected_ok
            );
            assert_eq!(
                rx.recv().await.unwrap(),
                format!("/v1/jobs/{id}/resolution")
            );
        }
    }

    #[tokio::test]
    async fn job_status_and_cancel_use_the_runtime_job_identifier() {
        let job_id = JobId::new();
        let (base, mut status_rx) = capture_uri(
            "/v1/jobs/{job}",
            axum::Json(json!({
                "id": job_id,
                "name": "fetch-report",
                "priority": "normal",
                "status": "running",
                "payload": null,
                "createdAt": "2026-09-19T00:00:00Z",
                "startedAt": "2026-09-19T00:00:01Z",
                "completedAt": null,
                "retryCount": 0,
                "maxRetries": 3,
                "result": null,
                "error": null,
                "timeoutMs": null,
                "correlationId": null,
            })),
        )
        .await;
        let client = BrowserRuntimeClient::new(base, "test-token").unwrap();
        let status = client.job_status(&job_id, None).await.unwrap();
        assert_eq!(status.id, job_id);
        assert_eq!(status.status, JobStatus::Running);
        assert_eq!(
            status_rx.recv().await.unwrap(),
            format!("/v1/jobs/{job_id}")
        );

        let (base, mut cancel_rx) = capture_uri("/v1/jobs/{job}", StatusCode::NO_CONTENT).await;
        let client = BrowserRuntimeClient::new(base, "test-token").unwrap();
        client.cancel_job(&job_id, None).await.unwrap();
        assert_eq!(
            cancel_rx.recv().await.unwrap(),
            format!("/v1/jobs/{job_id}")
        );
    }

    #[tokio::test]
    async fn submit_job_rejects_an_empty_name_before_transport() {
        let client = BrowserRuntimeClient::new("http://127.0.0.1:1", "test-token").unwrap();
        let error = client
            .submit_job(
                &SubmitJobRequest {
                    name: "  ".into(),
                    payload: None,
                    priority: None,
                    max_retries: None,
                    timeout_ms: None,
                },
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ClientError::Protocol(_)));
    }

    #[tokio::test]
    async fn job_helpers_require_their_exact_success_statuses() {
        let job_id = JobId::new();
        let app = Router::new().route(
            "/v1/jobs",
            axum::routing::post({
                let job_id = job_id.clone();
                move || async move { axum::Json(json!({ "jobId": job_id, "status": "pending" })) }
            }),
        );
        let client = BrowserRuntimeClient::new(spawn(app).await, "test-token").unwrap();
        let submit_error = client
            .submit_job(
                &SubmitJobRequest {
                    name: "fetch-report".into(),
                    payload: None,
                    priority: None,
                    max_retries: None,
                    timeout_ms: None,
                },
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(submit_error, ClientError::Protocol(_)));

        let (base, _) = capture_uri("/v1/jobs/{job}", StatusCode::OK).await;
        let client = BrowserRuntimeClient::new(base, "test-token").unwrap();
        let cancel_error = client.cancel_job(&job_id, None).await.unwrap_err();
        assert!(matches!(cancel_error, ClientError::Protocol(_)));
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
        let decision = client.recover(&workflow, None).await.unwrap();
        assert!(matches!(
            decision,
            RecoveryDecision::NeedsReconciliation { .. }
        ));
        assert_eq!(
            rx.recv().await.unwrap(),
            format!("/v1/recovery/{}", workflow.0)
        );
    }

    #[test]
    fn command_status_matches_all_broker_variants() {
        let id = CommandId::new();
        let attempt = AttemptId::new();
        let error =
            |code: &str| json!({"code":code,"message":"x","layer":"workflow","retryable":false});
        for (status, value) in [
            (
                200,
                json!({"status":"completed","commandId":id,"evidence":[]}),
            ),
            (
                200,
                json!({"status":"restarted","commandId":id,"priorAttemptId":attempt,"attemptId":attempt,"reason":"x","evidence":[]}),
            ),
            (
                503,
                json!({"status":"retryableFailure","commandId":id,"error":error("internal")}),
            ),
            (
                409,
                json!({"status":"needsReconciliation","commandId":id,"error":error("internal"),"evidence":[]}),
            ),
            (
                403,
                json!({"status":"policyDenied","commandId":id,"error":error("policyDenied")}),
            ),
            (
                429,
                json!({"status":"resourceExhausted","commandId":id,"error":error("resourceExhausted"),"retryAfterMs":1}),
            ),
            (
                422,
                json!({"status":"failed","commandId":id,"error":error("invalidRequest"),"evidence":[]}),
            ),
            (
                404,
                json!({"status":"failed","commandId":id,"error":error("notFound"),"evidence":[]}),
            ),
            (
                500,
                json!({"status":"failed","commandId":id,"error":error("internal"),"evidence":[]}),
            ),
        ] {
            let outcome: CommandOutcome = serde_json::from_value(value).unwrap();
            assert_eq!(command_status(&outcome).as_u16(), status);
        }
    }

    #[test]
    fn debug_never_prints_credentials() {
        let client =
            BrowserRuntimeClient::new("http://user:password@localhost:1", "secret-token").unwrap();
        let output = format!("{client:?}");
        assert!(!output.contains("secret-token"));
        assert!(!output.contains("password"));
    }

    #[tokio::test]
    async fn shared_outcome_fixtures_obey_status_and_preserve_evidence() {
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/sdk-responses.json")).unwrap();
        for (key, expected) in [("command", 409), ("recovery", 409)] {
            let value = fixtures[key].clone();
            for status in [expected, 418] {
                let (base, _) = capture_uri(
                    "/v1/test",
                    (
                        StatusCode::from_u16(status).unwrap(),
                        axum::Json(value.clone()),
                    ),
                )
                .await;
                let client = BrowserRuntimeClient::new(base, "test-token").unwrap();
                if key == "command" {
                    let result: Result<CommandOutcome, _> = client
                        .json_mapped(
                            Method::GET,
                            "/v1/test",
                            None::<()>,
                            None,
                            None,
                            Some(command_status),
                        )
                        .await;
                    if status == expected {
                        assert_eq!(serde_json::to_value(result.unwrap()).unwrap(), value);
                    } else {
                        assert!(matches!(result, Err(ClientError::Protocol(_))));
                    }
                } else {
                    let result: Result<RecoveryDecision, _> = client
                        .json_mapped(
                            Method::GET,
                            "/v1/test",
                            None::<()>,
                            None,
                            None,
                            Some(recovery_status_code),
                        )
                        .await;
                    if status == expected {
                        assert_eq!(serde_json::to_value(result.unwrap()).unwrap(), value);
                    } else {
                        assert!(matches!(result, Err(ClientError::Protocol(_))));
                    }
                }
            }
        }
        let (base, _) = capture_uri(
            "/v1/test",
            (
                StatusCode::CONFLICT,
                axum::Json(json!({"error":{"code":"invalidRequest"}})),
            ),
        )
        .await;
        let client = BrowserRuntimeClient::new(base, "test-token").unwrap();
        let result: Result<RecoveryDecision, _> = client
            .json_mapped(
                Method::GET,
                "/v1/test",
                None::<()>,
                None,
                None,
                Some(recovery_status_code),
            )
            .await;
        assert!(matches!(result, Err(ClientError::Http { status: 409, .. })));
    }

    #[tokio::test]
    async fn bounded_body_rejects_declared_and_streamed_excess() {
        for declared in [true, false] {
            let base = spawn_artifact_server("application/octet-stream", b"12345", declared).await;
            let response = reqwest::get(format!("{base}/v1/artifacts/a"))
                .await
                .unwrap();
            assert!(matches!(
                bounded_body(response, 4).await,
                Err(ClientError::Protocol(_))
            ));
        }
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

    #[tokio::test]
    async fn session_capabilities_validates_version_identity_and_feature_evidence() {
        let session = SessionId::new();
        let fixture = json!({
            "schemaVersion":1,"sessionId":session,"observedAt":"2026-10-15T00:00:00Z",
            "engine":"chromium","connection":"connected","features":[{
                "name":"captureScreenshot.viewport","supported":true,"available":true,"reason":null,
                "requiredCapabilities":["browser:mutate"],"policyRequirement":null
            }]
        });
        let payload = Arc::new(tokio::sync::Mutex::new(fixture.clone()));
        let response = payload.clone();
        let expected = session.0.to_string();
        let app = Router::new().route(
            "/v1/sessions/{session}/capabilities",
            get(
                move |axum::extract::Path(id): axum::extract::Path<String>| {
                    let response = response.clone();
                    let expected = expected.clone();
                    async move {
                        assert_eq!(id, expected);
                        axum::Json(response.lock().await.clone())
                    }
                },
            ),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = BrowserRuntimeClient::new(format!("http://{address}"), "test-token").unwrap();
        assert_eq!(
            client
                .session_capabilities(&session, None)
                .await
                .unwrap()
                .session_id,
            session
        );
        let mut invalids = Vec::new();
        let mut value = fixture.clone();
        value["schemaVersion"] = json!(2);
        invalids.push(value);
        let mut value = fixture.clone();
        value["sessionId"] = json!(SessionId::new());
        invalids.push(value);
        let mut value = fixture.clone();
        value["features"] = json!([]);
        invalids.push(value);
        let mut value = fixture.clone();
        value["features"] = json!([fixture["features"][0], fixture["features"][0]]);
        invalids.push(value);
        let mut value = fixture;
        value["connection"] = json!("fabricated");
        invalids.push(value);
        for value in invalids {
            *payload.lock().await = value;
            assert!(matches!(
                client.session_capabilities(&session, None).await,
                Err(ClientError::Protocol(_))
            ));
        }
        server.abort();
        let _ = server.await;
    }
}
