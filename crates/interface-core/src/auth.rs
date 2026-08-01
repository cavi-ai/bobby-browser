use std::{
    fmt,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::RwLock;
use types::{
    Capability, CapabilitySet, CorrelationId, ErrorLayer, IdempotencyKey, InterfaceError,
    InterfaceErrorCode, PrincipalId, RequestContext,
};

#[derive(Clone)]
struct AuthorityRecord {
    token_hash: [u8; 32],
    principal_id: PrincipalId,
    capabilities: CapabilitySet,
    expires_at: DateTime<Utc>,
    revoked: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct AuthorityStore {
    records: Arc<RwLock<Vec<AuthorityRecord>>>,
    capacity: usize,
}

impl Default for AuthorityStore {
    fn default() -> Self {
        Self::with_capacity(1024)
    }
}

impl fmt::Debug for AuthorityStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthorityStore")
            .field("credentials", &"[REDACTED]")
            .field("capacity", &self.capacity)
            .finish()
    }
}

impl AuthorityStore {
    pub fn in_memory() -> Self {
        Self::default()
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            records: Arc::new(RwLock::new(Vec::new())),
            capacity,
        }
    }

    pub async fn issue(
        &self,
        principal_id: PrincipalId,
        capabilities: impl IntoIterator<Item = Capability>,
        expires_at: DateTime<Utc>,
    ) -> Result<IssuedToken, InterfaceError> {
        let mut token_bytes = [0_u8; 32];
        getrandom::fill(&mut token_bytes).map_err(|_| authentication_error())?;
        let bearer = URL_SAFE_NO_PAD.encode(token_bytes);
        self.enroll_hash(
            sha256(bearer.as_bytes()),
            principal_id,
            capabilities,
            expires_at,
        )
        .await?;
        Ok(IssuedToken { bearer })
    }

    pub async fn enroll_hash(
        &self,
        token_hash: [u8; 32],
        principal_id: PrincipalId,
        capabilities: impl IntoIterator<Item = Capability>,
        expires_at: DateTime<Utc>,
    ) -> Result<CapabilityHandle, InterfaceError> {
        let now = Utc::now();
        if expires_at <= now {
            return Err(authentication_error());
        }
        let mut records = self.records.write().await;
        records.retain(|record| record.expires_at > now && !record.revoked.load(Ordering::Acquire));
        if records.len() >= self.capacity {
            return Err(resource_exhausted_error());
        }
        let record = AuthorityRecord {
            token_hash,
            principal_id,
            capabilities: CapabilitySet::new(capabilities),
            expires_at,
            revoked: Arc::new(AtomicBool::new(false)),
        };
        let handle = capability_handle(&record);
        records.push(record);
        Ok(handle)
    }

    pub async fn verify(&self, bearer: &str) -> Result<CapabilityHandle, InterfaceError> {
        self.authenticate(bearer, Utc::now()).await
    }
}

#[async_trait]
impl Authority for AuthorityStore {
    async fn authenticate(
        &self,
        bearer: &str,
        now: DateTime<Utc>,
    ) -> Result<CapabilityHandle, InterfaceError> {
        const MAX_AUTHENTICATION_INPUT_BYTES: usize = 4096;
        let input_within_bound = bearer.len() <= MAX_AUTHENTICATION_INPUT_BYTES;
        let candidate_hash = if input_within_bound {
            sha256(bearer.as_bytes())
        } else {
            sha256(&[])
        };
        let records = self.records.read().await;
        let mut matched = None;
        for record in records.iter() {
            let equal = record.token_hash.ct_eq(&candidate_hash);
            if bool::from(equal) && matched.is_none() {
                matched = Some(record);
            }
        }
        let Some(record) = matched else {
            return Err(authentication_error());
        };
        if !input_within_bound || record.revoked.load(Ordering::Acquire) || record.expires_at <= now
        {
            return Err(authentication_error());
        }
        Ok(capability_handle(record))
    }

    async fn revoke(&self, principal: &PrincipalId) -> Result<(), InterfaceError> {
        for record in self.records.write().await.iter_mut() {
            if record.principal_id == *principal {
                record.revoked.store(true, Ordering::Release);
            }
        }
        Ok(())
    }

    async fn issue(
        &self,
        principal: PrincipalId,
        capabilities: Vec<Capability>,
        expires_at: DateTime<Utc>,
    ) -> Result<IssuedToken, InterfaceError> {
        AuthorityStore::issue(self, principal, capabilities, expires_at).await
    }
}

#[async_trait]
pub trait Authority: Send + Sync {
    async fn authenticate(
        &self,
        bearer: &str,
        now: DateTime<Utc>,
    ) -> Result<CapabilityHandle, InterfaceError>;
    async fn revoke(&self, principal: &PrincipalId) -> Result<(), InterfaceError>;

    async fn issue(
        &self,
        _principal: PrincipalId,
        _capabilities: Vec<Capability>,
        _expires_at: DateTime<Utc>,
    ) -> Result<IssuedToken, InterfaceError> {
        Err(InterfaceError {
            code: InterfaceErrorCode::UnsupportedOperation,
            layer: ErrorLayer::Interface,
            message: "principal issuance is not supported by this authority".to_owned(),
            correlation_id: CorrelationId::new(),
            command_id: None,
            retryable: false,
            retry_after_ms: None,
            reconciliation_required: false,
            required_capability: None,
        })
    }
}

pub struct IssuedToken {
    bearer: String,
}

impl IssuedToken {
    pub fn expose_once(self) -> String {
        self.bearer
    }

    /// Authority-internal constructor for callers that mint their own bearer outside
    /// `AuthorityStore::issue` (e.g. a persistent `Authority` implementation that must
    /// generate the bearer itself before enrolling only its hash). Not for use outside
    /// an `Authority` implementation — never construct an `IssuedToken` from bearer
    /// material that has not already been enrolled by hash.
    pub fn from_bearer(bearer: String) -> Self {
        Self { bearer }
    }
}

impl fmt::Debug for IssuedToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("IssuedToken([REDACTED])")
    }
}

#[derive(Clone)]
pub struct CapabilityHandle {
    principal_id: PrincipalId,
    capabilities: CapabilitySet,
    expires_at: DateTime<Utc>,
    revoked: Arc<AtomicBool>,
}

impl CapabilityHandle {
    pub fn context(&self, deadline: DateTime<Utc>, key: Option<IdempotencyKey>) -> RequestContext {
        RequestContext {
            interface_version: types::InterfaceVersion::CURRENT,
            correlation_id: CorrelationId::new(),
            principal_id: self.principal_id.clone(),
            capabilities: self.capabilities.clone(),
            deadline,
            idempotency_key: key,
        }
    }

    pub fn principal_id(&self) -> &PrincipalId {
        &self.principal_id
    }

    /// The capability set this handle carries. Exposed so callers that cache a runtime
    /// binding across requests (see `broker::bootstrap_listener_with`) can detect that a
    /// later request's handle grants a different capability set than the cached one and
    /// rebuild the binding, rather than silently authorizing against a stale set.
    pub fn capabilities(&self) -> &CapabilitySet {
        &self.capabilities
    }

    pub(crate) fn allows(&self, capability: Capability) -> bool {
        self.capabilities.contains(capability)
    }

    pub fn is_valid_at(&self, now: DateTime<Utc>) -> bool {
        !self.is_invalid_at(now)
    }

    /// When this handle stops authorizing, so a caller can say so before it
    /// happens.
    ///
    /// The MCP stdio gateway pins its bootstrap expiry into client config and
    /// refuses to start once it passes, which an agent host surfaces only as a
    /// dead server. Reporting the instant is the difference between "renew the
    /// credential" and an unexplained disconnect. Expiry is not a secret — it
    /// carries no bearer material.
    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }

    pub(crate) fn is_invalid_at(&self, now: DateTime<Utc>) -> bool {
        self.expires_at <= now || self.revoked.load(Ordering::Acquire)
    }
}

impl fmt::Debug for CapabilityHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CapabilityHandle([REDACTED])")
    }
}

fn capability_handle(record: &AuthorityRecord) -> CapabilityHandle {
    CapabilityHandle {
        principal_id: record.principal_id.clone(),
        capabilities: record.capabilities.clone(),
        expires_at: record.expires_at,
        revoked: record.revoked.clone(),
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn authentication_error() -> InterfaceError {
    InterfaceError {
        code: InterfaceErrorCode::AuthenticationFailed,
        layer: ErrorLayer::Interface,
        message: "authentication failed".to_owned(),
        correlation_id: CorrelationId::new(),
        command_id: None,
        retryable: false,
        retry_after_ms: None,
        reconciliation_required: false,
        required_capability: None,
    }
}

fn resource_exhausted_error() -> InterfaceError {
    InterfaceError {
        code: InterfaceErrorCode::ResourceExhausted,
        layer: ErrorLayer::Interface,
        message: "authority capacity exhausted".to_owned(),
        correlation_id: CorrelationId::new(),
        command_id: None,
        retryable: false,
        retry_after_ms: None,
        reconciliation_required: false,
        required_capability: None,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use types::{Capability, PrincipalId};
    use uuid::Uuid;

    use super::AuthorityStore;

    #[tokio::test]
    async fn rejected_expired_hash_is_never_retained() {
        let store = AuthorityStore::with_capacity(1);
        assert!(store
            .enroll_hash(
                [7; 32],
                PrincipalId::from_uuid(Uuid::nil()),
                [Capability::SessionRead],
                Utc::now() - Duration::seconds(1),
            )
            .await
            .is_err());
        assert!(store.records.read().await.is_empty());
    }
}
