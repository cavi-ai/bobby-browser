use std::{
    fmt,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use axum::{
    extract::{Request, State},
    http::{header::WWW_AUTHENTICATE, HeaderMap, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use interface_core::{Authority, AuthorityStore, AuthorizationGuard, CapabilityHandle};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use types::{
    Capability, CorrelationId, ErrorLayer, IdempotencyKey, InterfaceError, InterfaceErrorCode,
    InterfaceVersion, PrincipalId, RequestContext,
};
use uuid::Uuid;

use crate::AppState;

const MAX_AUTHORIZATION_BYTES: usize = 512;
const MAX_VERSION_BYTES: usize = 64;
const MAX_CORRELATION_BYTES: usize = 64;
const MAX_DEADLINE_BYTES: usize = 64;
const MAX_DEADLINE_AHEAD_MINUTES: i64 = 5;

#[derive(Clone)]
pub(crate) struct AuthenticatedRequest {
    pub handle: CapabilityHandle,
    pub context: RequestContext,
    pub runtime: Arc<dyn interface_core::RuntimeInterface>,
}

pub struct StartupCredential {
    bearer: String,
    principal_id: PrincipalId,
    capabilities: Vec<Capability>,
    expires_at: DateTime<Utc>,
}

impl StartupCredential {
    pub fn new(
        bearer: String,
        principal_id: PrincipalId,
        capabilities: Vec<Capability>,
        expires_at: DateTime<Utc>,
    ) -> Result<Self, StartupCredentialError> {
        if !valid_bearer(&bearer) {
            return Err(StartupCredentialError::InvalidBearer);
        }
        if capabilities.is_empty() {
            return Err(StartupCredentialError::MissingCapabilities);
        }
        if expires_at <= Utc::now() {
            return Err(StartupCredentialError::Expired);
        }
        Ok(Self {
            bearer,
            principal_id,
            capabilities,
            expires_at,
        })
    }

    pub fn from_env() -> Result<Self, StartupCredentialError> {
        let bearer = required_env("AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN")?;
        let principal = required_env("AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL")?;
        let capabilities = required_env("AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES")?;
        let expires_at = required_env("AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT")?;
        let principal_id = PrincipalId::from_uuid(
            Uuid::parse_str(&principal).map_err(|_| StartupCredentialError::InvalidPrincipal)?,
        );
        let capabilities = capabilities
            .split(',')
            .map(str::trim)
            .map(parse_capability)
            .collect::<Result<Vec<_>, _>>()?;
        let expires_at = DateTime::parse_from_rfc3339(&expires_at)
            .map_err(|_| StartupCredentialError::InvalidExpiry)?
            .with_timezone(&Utc);
        Self::new(bearer, principal_id, capabilities, expires_at)
    }
}

impl fmt::Debug for StartupCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StartupCredential")
            .field("bearer", &"[REDACTED]")
            .field("principal_id", &self.principal_id)
            .field("capabilities", &self.capabilities)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupCredentialError {
    MissingInput,
    InvalidBearer,
    InvalidPrincipal,
    MissingCapabilities,
    InvalidCapability,
    InvalidExpiry,
    Expired,
    EnrollmentFailed,
}

impl fmt::Display for StartupCredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MissingInput => "explicit startup authority input is required",
            Self::InvalidBearer => "startup bearer is invalid",
            Self::InvalidPrincipal => "startup principal is invalid",
            Self::MissingCapabilities => "startup capabilities must be explicit and nonempty",
            Self::InvalidCapability => "startup capability is invalid",
            Self::InvalidExpiry => "startup expiry is invalid",
            Self::Expired => "startup credential is expired",
            Self::EnrollmentFailed => "startup authority enrollment failed",
        })
    }
}

impl std::error::Error for StartupCredentialError {}

#[derive(Clone)]
pub struct EnrolledAuthority {
    token_hash: [u8; 32],
    principal_id: PrincipalId,
    expires_at: DateTime<Utc>,
    handle: CapabilityHandle,
    store: AuthorityStore,
    revoked: Arc<AtomicBool>,
}

impl EnrolledAuthority {
    pub async fn enroll(startup: StartupCredential) -> Result<Self, StartupCredentialError> {
        let token_hash = Sha256::digest(startup.bearer.as_bytes()).into();
        let store = AuthorityStore::with_capacity(1);
        let internal = store
            .issue(
                startup.principal_id.clone(),
                startup.capabilities,
                startup.expires_at,
            )
            .await
            .map_err(|_| StartupCredentialError::EnrollmentFailed)?
            .expose_once();
        let handle = store
            .verify(&internal)
            .await
            .map_err(|_| StartupCredentialError::EnrollmentFailed)?;
        Ok(Self {
            token_hash,
            principal_id: startup.principal_id,
            expires_at: startup.expires_at,
            handle,
            store,
            revoked: Arc::new(AtomicBool::new(false)),
        })
    }
}

impl fmt::Debug for EnrolledAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnrolledAuthority")
            .field("credential", &"[REDACTED]")
            .finish()
    }
}

#[async_trait::async_trait]
impl Authority for EnrolledAuthority {
    async fn authenticate(
        &self,
        bearer: &str,
        now: DateTime<Utc>,
    ) -> Result<CapabilityHandle, InterfaceError> {
        let candidate: [u8; 32] = Sha256::digest(bearer.as_bytes()).into();
        if !valid_bearer(bearer)
            || !bool::from(self.token_hash.ct_eq(&candidate))
            || self.expires_at <= now
            || self.revoked.load(Ordering::Acquire)
        {
            return Err(authentication_error(CorrelationId::new()));
        }
        Ok(self.handle.clone())
    }

    async fn revoke(&self, principal: &PrincipalId) -> Result<(), InterfaceError> {
        if principal == &self.principal_id {
            self.revoked.store(true, Ordering::Release);
            self.store.revoke(principal).await?;
        }
        Ok(())
    }
}

pub(crate) async fn authenticate(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let bearer = match bearer(request.headers()) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let parsed = match parse_context_headers(request.headers()) {
        Ok(parsed) => parsed,
        Err(error) => return error.into_response(),
    };
    let handle = match state.authority.authenticate(&bearer, Utc::now()).await {
        Ok(handle) => handle,
        Err(mut error) => {
            error.correlation_id = parsed.correlation_id;
            return ProtocolError::from(error).into_response();
        }
    };
    let mut context = handle.context(parsed.deadline, parsed.idempotency_key);
    context.interface_version = parsed.interface_version;
    context.correlation_id = parsed.correlation_id;
    let correlation_header = serde_json::to_value(&context.correlation_id)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .and_then(|value| HeaderValue::from_str(&value).ok());
    let runtime = (state.bind_runtime)(handle.clone());
    request.extensions_mut().insert(AuthenticatedRequest {
        handle,
        context,
        runtime,
    });

    let Ok(_permit) = state.connections.clone().try_acquire_owned() else {
        return ProtocolError::from(interface_error(
            InterfaceErrorCode::ResourceExhausted,
            "interface connection capacity exhausted",
            request
                .extensions()
                .get::<AuthenticatedRequest>()
                .unwrap()
                .context
                .correlation_id
                .clone(),
            Some(1_000),
        ))
        .into_response();
    };
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        "x-interface-version",
        HeaderValue::from_static(types::CURRENT_INTERFACE_VERSION),
    );
    if let Some(correlation) = correlation_header {
        response
            .headers_mut()
            .insert("x-correlation-id", correlation);
    }
    response
}

struct ParsedHeaders {
    interface_version: InterfaceVersion,
    correlation_id: CorrelationId,
    deadline: DateTime<Utc>,
    idempotency_key: Option<IdempotencyKey>,
}

fn bearer(headers: &HeaderMap) -> Result<String, ProtocolError> {
    let value = exactly_one(headers, "authorization", MAX_AUTHORIZATION_BYTES)
        .map_err(|_| ProtocolError::authentication())?;
    let value = value
        .to_str()
        .map_err(|_| ProtocolError::authentication())?;
    let Some(value) = value.strip_prefix("Bearer ") else {
        return Err(ProtocolError::authentication());
    };
    if !valid_bearer(value) {
        return Err(ProtocolError::authentication());
    }
    Ok(value.to_owned())
}

fn parse_context_headers(headers: &HeaderMap) -> Result<ParsedHeaders, ProtocolError> {
    let correlation = required_text(headers, "x-correlation-id", MAX_CORRELATION_BYTES)
        .map_err(|_| ProtocolError::invalid(InterfaceErrorCode::InvalidRequest))?;
    let correlation_id = CorrelationId::from_uuid(
        Uuid::parse_str(correlation)
            .map_err(|_| ProtocolError::invalid(InterfaceErrorCode::InvalidRequest))?,
    );
    let version =
        required_text(headers, "x-interface-version", MAX_VERSION_BYTES).map_err(|_| {
            ProtocolError::invalid_with(
                InterfaceErrorCode::UnsupportedInterfaceVersion,
                correlation_id.clone(),
            )
        })?;
    let interface_version = InterfaceVersion::try_from(version).map_err(|_| {
        ProtocolError::invalid_with(
            InterfaceErrorCode::UnsupportedInterfaceVersion,
            correlation_id.clone(),
        )
    })?;
    let deadline = required_text(headers, "x-deadline", MAX_DEADLINE_BYTES).map_err(|_| {
        ProtocolError::invalid_with(InterfaceErrorCode::InvalidRequest, correlation_id.clone())
    })?;
    let deadline = DateTime::parse_from_rfc3339(deadline)
        .map_err(|_| {
            ProtocolError::invalid_with(InterfaceErrorCode::InvalidRequest, correlation_id.clone())
        })?
        .with_timezone(&Utc);
    let now = Utc::now();
    if deadline <= now {
        return Err(ProtocolError::from(interface_error(
            InterfaceErrorCode::DeadlineExceeded,
            "request deadline exceeded",
            correlation_id,
            None,
        )));
    }
    if deadline > now + chrono::Duration::minutes(MAX_DEADLINE_AHEAD_MINUTES) {
        return Err(ProtocolError::invalid_with(
            InterfaceErrorCode::InvalidRequest,
            correlation_id,
        ));
    }
    let idempotency_key = optional_text(headers, "idempotency-key", 128)
        .map_err(|_| {
            ProtocolError::invalid_with(
                InterfaceErrorCode::InvalidIdempotencyKey,
                correlation_id.clone(),
            )
        })?
        .map(IdempotencyKey::try_from)
        .transpose()
        .map_err(|_| {
            ProtocolError::invalid_with(
                InterfaceErrorCode::InvalidIdempotencyKey,
                correlation_id.clone(),
            )
        })?;
    Ok(ParsedHeaders {
        interface_version,
        correlation_id,
        deadline,
        idempotency_key,
    })
}

fn exactly_one<'a>(
    headers: &'a HeaderMap,
    name: &str,
    max_bytes: usize,
) -> Result<&'a HeaderValue, ()> {
    let mut values = headers.get_all(name).iter();
    let value = values.next().ok_or(())?;
    if values.next().is_some() || value.as_bytes().len() > max_bytes {
        return Err(());
    }
    Ok(value)
}

fn required_text<'a>(headers: &'a HeaderMap, name: &str, max_bytes: usize) -> Result<&'a str, ()> {
    exactly_one(headers, name, max_bytes).and_then(|value| value.to_str().map_err(|_| ()))
}

fn optional_text<'a>(
    headers: &'a HeaderMap,
    name: &str,
    max_bytes: usize,
) -> Result<Option<&'a str>, ()> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() || value.as_bytes().len() > max_bytes {
        return Err(());
    }
    value.to_str().map(Some).map_err(|_| ())
}

fn valid_bearer(value: &str) -> bool {
    (32..=MAX_AUTHORIZATION_BYTES).contains(&value.len())
        && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

fn required_env(name: &'static str) -> Result<String, StartupCredentialError> {
    std::env::var(name).map_err(|_| StartupCredentialError::MissingInput)
}

fn parse_capability(value: &str) -> Result<Capability, StartupCredentialError> {
    match value {
        "session:read" => Ok(Capability::SessionRead),
        "session:write" => Ok(Capability::SessionWrite),
        "page:read" => Ok(Capability::PageRead),
        "page:write" => Ok(Capability::PageWrite),
        "browser:mutate" => Ok(Capability::BrowserMutate),
        "file:upload" => Ok(Capability::FileUpload),
        "file:download" => Ok(Capability::FileDownload),
        "javascript:evaluate" => Ok(Capability::JavascriptEvaluate),
        "artifact:read" => Ok(Capability::ArtifactRead),
        "artifact:capture" => Ok(Capability::ArtifactCapture),
        "recovery:read" => Ok(Capability::RecoveryRead),
        "recovery:write" => Ok(Capability::RecoveryWrite),
        _ => Err(StartupCredentialError::InvalidCapability),
    }
}

pub(crate) struct ProtocolError {
    error: InterfaceError,
    status_override: Option<StatusCode>,
}

impl ProtocolError {
    pub fn authentication() -> Self {
        Self::from(authentication_error(CorrelationId::new()))
    }

    pub fn invalid(code: InterfaceErrorCode) -> Self {
        Self::invalid_with(code, CorrelationId::new())
    }

    pub fn invalid_with(code: InterfaceErrorCode, correlation_id: CorrelationId) -> Self {
        Self::from(interface_error(
            code,
            "interface request is invalid",
            correlation_id,
            None,
        ))
    }

    pub fn oversized(correlation_id: CorrelationId) -> Self {
        Self {
            error: interface_error(
                InterfaceErrorCode::InvalidRequest,
                "request body exceeds the configured bound",
                correlation_id,
                None,
            ),
            status_override: Some(StatusCode::PAYLOAD_TOO_LARGE),
        }
    }
}

impl From<InterfaceError> for ProtocolError {
    fn from(error: InterfaceError) -> Self {
        Self {
            error,
            status_override: None,
        }
    }
}

impl IntoResponse for ProtocolError {
    fn into_response(self) -> Response {
        let authenticate = matches!(
            self.error.code,
            InterfaceErrorCode::AuthenticationFailed | InterfaceErrorCode::TokenExpired
        );
        let status = self
            .status_override
            .unwrap_or_else(|| error_status(&self.error));
        let retry_after_ms = self.error.retry_after_ms;
        let mut response =
            (status, Json(serde_json::json!({ "error": self.error }))).into_response();
        if authenticate {
            response
                .headers_mut()
                .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        if let Some(milliseconds) = retry_after_ms {
            let seconds = milliseconds.saturating_add(999) / 1_000;
            if let Ok(value) = HeaderValue::from_str(&seconds.max(1).to_string()) {
                response.headers_mut().insert("retry-after", value);
            }
        }
        response
    }
}

fn error_status(error: &InterfaceError) -> StatusCode {
    if error.reconciliation_required {
        return StatusCode::CONFLICT;
    }
    match error.code {
        InterfaceErrorCode::AuthenticationFailed | InterfaceErrorCode::TokenExpired => {
            StatusCode::UNAUTHORIZED
        }
        InterfaceErrorCode::MissingCapability | InterfaceErrorCode::MalformedScope => {
            StatusCode::FORBIDDEN
        }
        InterfaceErrorCode::ArtifactDenied | InterfaceErrorCode::NotFound => StatusCode::NOT_FOUND,
        InterfaceErrorCode::DeadlineExceeded => StatusCode::REQUEST_TIMEOUT,
        InterfaceErrorCode::IdempotencyConflict => StatusCode::CONFLICT,
        InterfaceErrorCode::ResourceExhausted => StatusCode::TOO_MANY_REQUESTS,
        InterfaceErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        InterfaceErrorCode::InvalidRequest
        | InterfaceErrorCode::UnsupportedInterfaceVersion
        | InterfaceErrorCode::InvalidIdempotencyKey
        | InterfaceErrorCode::UnsupportedOperation => StatusCode::UNPROCESSABLE_ENTITY,
    }
}

fn authentication_error(correlation_id: CorrelationId) -> InterfaceError {
    interface_error(
        InterfaceErrorCode::AuthenticationFailed,
        "authentication failed",
        correlation_id,
        None,
    )
}

pub(crate) fn interface_error(
    code: InterfaceErrorCode,
    message: &str,
    correlation_id: CorrelationId,
    retry_after_ms: Option<u64>,
) -> InterfaceError {
    InterfaceError {
        code,
        layer: ErrorLayer::Interface,
        message: message.to_owned(),
        correlation_id,
        command_id: None,
        retryable: retry_after_ms.is_some(),
        retry_after_ms,
        reconciliation_required: false,
        required_capability: None,
    }
}

pub(crate) fn authorize_boundary(
    request: &AuthenticatedRequest,
    operation: types::InterfaceOperation,
) -> Result<(), ProtocolError> {
    AuthorizationGuard::new(request.handle.clone())
        .authorize(&request.context, operation)
        .map_err(ProtocolError::from)
}
