use std::{fmt, sync::Arc};

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
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use types::{
    Capability, CorrelationId, ErrorLayer, IdempotencyKey, InterfaceError, InterfaceErrorCode,
    InterfaceVersion, PrincipalId, RequestContext,
};
use uuid::Uuid;

use crate::AppState;

const MAX_AUTHORIZATION_HEADER_BYTES: usize = 512;
const BEARER_PREFIX_BYTES: usize = "Bearer ".len();
const MAX_BEARER_BYTES: usize = MAX_AUTHORIZATION_HEADER_BYTES - BEARER_PREFIX_BYTES;
const MIN_BEARER_BYTES: usize = 32;
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
    token_hash: [u8; 32],
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
        let token_hash = Sha256::digest(bearer.as_bytes()).into();
        Ok(Self {
            token_hash,
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
            .field("token_hash", &"[REDACTED]")
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
    store: AuthorityStore,
    startup_handle: CapabilityHandle,
}

impl EnrolledAuthority {
    pub async fn enroll(
        startup: StartupCredential,
        max_principals: usize,
    ) -> Result<Self, StartupCredentialError> {
        // `+ 1` reserves a slot for the startup/admin credential itself, so
        // `max_principals` means "how many *issued* team principals fit" rather than
        // being silently reduced by one to make room for the startup credential that
        // every `EnrolledAuthority` already carries.
        let store = AuthorityStore::with_capacity(max_principals + 1);
        let startup_handle = store
            .enroll_hash(
                startup.token_hash,
                startup.principal_id,
                startup.capabilities,
                startup.expires_at,
            )
            .await
            .map_err(|_| StartupCredentialError::EnrollmentFailed)?;
        Ok(Self {
            store,
            startup_handle,
        })
    }

    pub(crate) fn startup_handle(&self) -> CapabilityHandle {
        self.startup_handle.clone()
    }

    pub async fn issue(
        &self,
        principal: PrincipalId,
        capabilities: Vec<Capability>,
        expires_at: DateTime<Utc>,
    ) -> Result<interface_core::IssuedToken, InterfaceError> {
        self.store.issue(principal, capabilities, expires_at).await
    }

    /// Re-enrolls a record whose bearer already exists elsewhere (restored from disk by
    /// `PersistentAuthority`, or a freshly generated bearer whose hash the caller already
    /// computed) directly by hash, discarding the resulting `CapabilityHandle`. Delegates
    /// to `AuthorityStore::enroll_hash`, so it is still subject to the same expiry check
    /// and capacity bound as any other enrollment.
    pub(crate) async fn enroll_restored(
        &self,
        hash: [u8; 32],
        principal: PrincipalId,
        capabilities: Vec<Capability>,
        expires_at: DateTime<Utc>,
    ) -> Result<(), InterfaceError> {
        self.store
            .enroll_hash(hash, principal, capabilities, expires_at)
            .await
            .map(|_handle| ())
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
        if !valid_bearer(bearer) {
            return Err(authentication_error(CorrelationId::new()));
        }
        self.store.authenticate(bearer, now).await
    }

    async fn revoke(&self, principal: &PrincipalId) -> Result<(), InterfaceError> {
        self.store.revoke(principal).await
    }

    async fn issue(
        &self,
        principal: PrincipalId,
        capabilities: Vec<Capability>,
        expires_at: DateTime<Utc>,
    ) -> Result<interface_core::IssuedToken, InterfaceError> {
        EnrolledAuthority::issue(self, principal, capabilities, expires_at).await
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
    let handle = match state.authority.authenticate(&bearer, Utc::now()).await {
        Ok(handle) => handle,
        Err(error) => return ProtocolError::from(error).into_response(),
    };
    let parsed = match parse_context_headers(request.headers()) {
        Ok(parsed) => parsed,
        Err(error) => return error.into_response(),
    };
    let principal_id = handle.principal_id().clone();
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

    let correlation_id = request
        .extensions()
        .get::<AuthenticatedRequest>()
        .expect("authenticated request context is present")
        .context
        .correlation_id
        .clone();
    let mut response = match state.in_flight_requests.clone().try_acquire_owned() {
        Err(_) => ProtocolError::from(interface_error(
            InterfaceErrorCode::ResourceExhausted,
            "interface in-flight request capacity exhausted",
            correlation_id.clone(),
            Some(1_000),
        ))
        .into_response(),
        // `_global_permit` and `_principal_permit` are both held across
        // `next.run(request).await` below: they are dropped only once this match arm's
        // value (the response) has been produced.
        Ok(_global_permit) => {
            match acquire_principal_permit(&state, &principal_id, correlation_id.clone()).await {
                Err(error) => error.into_response(),
                Ok(_principal_permit) => {
                    match crate::routes::validate_request_boundary(&state, &mut request).await {
                        Err(error) => error.into_response(),
                        Ok(()) => next.run(request).await,
                    }
                }
            }
        }
    };
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

/// Acquires an owned permit from `principal`'s per-principal in-flight semaphore,
/// creating it (bounded by `state.interface.max_in_flight_per_principal`) on first
/// use. This keeps one team's request burst from starving every other principal's
/// share of the interface's global in-flight capacity.
pub(crate) async fn acquire_principal_permit(
    state: &AppState,
    principal: &PrincipalId,
    correlation_id: CorrelationId,
) -> Result<OwnedSemaphorePermit, ProtocolError> {
    let existing = state.principal_permits.read().await.get(principal).cloned();
    let semaphore = match existing {
        Some(semaphore) => semaphore,
        None => state
            .principal_permits
            .write()
            .await
            .entry(principal.clone())
            .or_insert_with(|| {
                Arc::new(Semaphore::new(state.interface.max_in_flight_per_principal))
            })
            .clone(),
    };
    semaphore.try_acquire_owned().map_err(|_| {
        ProtocolError::from(interface_error(
            InterfaceErrorCode::ResourceExhausted,
            "principal in-flight capacity exhausted",
            correlation_id,
            Some(1_000),
        ))
    })
}

struct ParsedHeaders {
    interface_version: InterfaceVersion,
    correlation_id: CorrelationId,
    deadline: DateTime<Utc>,
    idempotency_key: Option<IdempotencyKey>,
}

fn bearer(headers: &HeaderMap) -> Result<String, ProtocolError> {
    let value = exactly_one(headers, "authorization", MAX_AUTHORIZATION_HEADER_BYTES)
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
    (MIN_BEARER_BYTES..=MAX_BEARER_BYTES).contains(&value.len())
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
        "authority:admin" => Ok(Capability::AuthorityAdmin),
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn startup() -> StartupCredential {
        StartupCredential::new(
            "bootstrap-bearer-0123456789abcdef0123456789abcdef".to_owned(),
            PrincipalId::from_uuid(Uuid::nil()),
            vec![Capability::AuthorityAdmin, Capability::SessionRead],
            Utc::now() + Duration::minutes(30),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn enrolled_authority_issues_second_principal() {
        let authority = EnrolledAuthority::enroll(startup(), 4).await.unwrap();
        let issued = authority
            .issue(
                PrincipalId::from_uuid(Uuid::from_u128(2)),
                vec![Capability::SessionRead, Capability::SessionWrite],
                Utc::now() + Duration::minutes(10),
            )
            .await
            .unwrap();
        let bearer = issued.expose_once();
        let handle = authority.authenticate(&bearer, Utc::now()).await.unwrap();
        assert!(handle.is_valid_at(Utc::now()));
    }

    #[tokio::test]
    async fn capacity_still_bounds_enrollment() {
        // `max_principals: 1` reserves capacity for exactly one *issued* principal, on
        // top of the startup credential's own reserved slot (see the `+ 1` in
        // `EnrolledAuthority::enroll`): the first issuance must succeed and the second
        // must fail.
        let authority = EnrolledAuthority::enroll(startup(), 1).await.unwrap();
        assert!(authority
            .issue(
                PrincipalId::from_uuid(Uuid::from_u128(3)),
                vec![Capability::SessionRead],
                Utc::now() + Duration::minutes(10),
            )
            .await
            .is_ok());
        assert!(authority
            .issue(
                PrincipalId::from_uuid(Uuid::from_u128(4)),
                vec![Capability::SessionRead],
                Utc::now() + Duration::minutes(10),
            )
            .await
            .is_err());
    }
}
