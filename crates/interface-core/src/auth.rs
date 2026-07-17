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
        let now = Utc::now();
        let mut records = self.records.write().await;
        records.retain(|record| record.expires_at > now && !record.revoked.load(Ordering::Acquire));
        if records.len() >= self.capacity {
            return Err(resource_exhausted_error());
        }
        let mut token_bytes = [0_u8; 32];
        getrandom::fill(&mut token_bytes).map_err(|_| authentication_error())?;
        let token_hash = sha256(token_bytes);
        records.push(AuthorityRecord {
            token_hash,
            principal_id,
            capabilities: CapabilitySet::new(capabilities),
            expires_at,
            revoked: Arc::new(AtomicBool::new(false)),
        });
        Ok(IssuedToken {
            bearer: URL_SAFE_NO_PAD.encode(token_bytes),
        })
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
        let mut token_bytes = [0_u8; 32];
        let decoded = URL_SAFE_NO_PAD.decode(bearer.as_bytes()).ok();
        let structurally_valid = decoded.as_ref().is_some_and(|value| value.len() == 32);
        if let Some(value) = decoded.filter(|value| value.len() == 32) {
            token_bytes.copy_from_slice(&value);
        }
        let candidate_hash = sha256(token_bytes);
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
        if !structurally_valid || record.revoked.load(Ordering::Acquire) || record.expires_at <= now
        {
            return Err(authentication_error());
        }
        Ok(CapabilityHandle {
            principal_id: record.principal_id.clone(),
            capabilities: record.capabilities.clone(),
            expires_at: record.expires_at,
            revoked: record.revoked.clone(),
        })
    }

    async fn revoke(&self, principal: &PrincipalId) -> Result<(), InterfaceError> {
        for record in self.records.write().await.iter_mut() {
            if record.principal_id == *principal {
                record.revoked.store(true, Ordering::Release);
            }
        }
        Ok(())
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
}

pub struct IssuedToken {
    bearer: String,
}

impl IssuedToken {
    pub fn expose_once(self) -> String {
        self.bearer
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

    pub(crate) fn principal_id(&self) -> &PrincipalId {
        &self.principal_id
    }

    pub(crate) fn allows(&self, capability: Capability) -> bool {
        self.capabilities.contains(capability)
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

fn sha256(bytes: [u8; 32]) -> [u8; 32] {
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
