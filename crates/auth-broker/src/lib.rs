//! Provider-neutral authentication lifecycle for vision backends.

use std::any::Any;
use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

const MAX_PROFILE_ID_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AuthProfileId(String);

impl AuthProfileId {
    pub fn new(value: impl Into<String>) -> Result<Self, AuthError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_PROFILE_ID_BYTES
            || value.chars().any(char::is_control)
        {
            return Err(AuthError::InvalidProfileId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AuthStrategy {
    Advertised,
    OAuthAuthorizationCode,
    OAuthDeviceCode,
    Environment,
    ExistingSession,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthCapabilities {
    strategies: BTreeSet<AuthStrategy>,
}

impl AuthCapabilities {
    pub fn new(strategies: impl IntoIterator<Item = AuthStrategy>) -> Self {
        Self {
            strategies: strategies.into_iter().collect(),
        }
    }

    pub fn supports(&self, strategy: AuthStrategy) -> bool {
        self.strategies.contains(&strategy)
    }

    pub fn strategies(&self) -> impl Iterator<Item = AuthStrategy> + '_ {
        self.strategies.iter().copied()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthChallenge {
    pub id: String,
    pub strategy: AuthStrategy,
    pub verification_uri: Option<String>,
    pub user_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthInput {
    AuthorizationCode(String),
    DeviceAcknowledged,
    Empty,
}

#[derive(Clone)]
pub struct CredentialHandle {
    profile: AuthProfileId,
    strategy: AuthStrategy,
    payload: Arc<dyn Any + Send + Sync>,
}

impl CredentialHandle {
    pub fn new(
        profile: AuthProfileId,
        strategy: AuthStrategy,
        payload: Arc<dyn Any + Send + Sync>,
    ) -> Self {
        Self {
            profile,
            strategy,
            payload,
        }
    }

    pub fn profile(&self) -> &AuthProfileId {
        &self.profile
    }

    pub fn strategy(&self) -> AuthStrategy {
        self.strategy
    }

    pub fn payload<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.payload.downcast_ref()
    }
}

impl fmt::Debug for CredentialHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialHandle(REDACTED)")
    }
}

#[derive(Debug, Clone)]
pub enum AuthProgress {
    Pending(AuthChallenge),
    Authenticated(CredentialHandle),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStatus {
    Healthy,
    PendingUserAction,
    ReauthenticationRequired,
    Revoked,
    Unavailable,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuthError {
    #[error("authentication profile id must contain 1..=64 non-control bytes")]
    InvalidProfileId,
    #[error("authentication transition is invalid for the current state")]
    InvalidTransition,
    #[error("authentication strategy is not supported")]
    UnsupportedStrategy,
    #[error("authentication was rejected")]
    Rejected,
    #[error("authentication transport failed: {0}")]
    Transport(String),
}

#[async_trait]
pub trait AuthDriver: Send + Sync {
    async fn discover(&self, profile: &AuthProfileId) -> Result<AuthCapabilities, AuthError>;

    async fn begin(
        &self,
        profile: &AuthProfileId,
        strategy: AuthStrategy,
    ) -> Result<AuthProgress, AuthError>;

    async fn continue_auth(
        &self,
        challenge: &AuthChallenge,
        input: AuthInput,
    ) -> Result<AuthProgress, AuthError>;

    async fn refresh(&self, handle: &CredentialHandle) -> Result<AuthProgress, AuthError>;

    async fn revoke(&self, handle: CredentialHandle) -> Result<(), AuthError>;

    async fn health(&self, handle: &CredentialHandle) -> AuthStatus;
}
