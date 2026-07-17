use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    ArtifactId, Capability, CapabilitySet, CommandId, CommandOutcome, ErrorLayer, PrincipalId,
};

pub const CURRENT_INTERFACE_VERSION: &str = "2026-07-17";

/// The only interface version accepted by this phase's wire contract.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct InterfaceVersion;

impl InterfaceVersion {
    pub const CURRENT: Self = Self;
}

impl TryFrom<&str> for InterfaceVersion {
    type Error = InterfaceValidationError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        if value == CURRENT_INTERFACE_VERSION {
            Ok(Self)
        } else {
            Err(InterfaceValidationError::UnsupportedInterfaceVersion(
                value.to_owned(),
            ))
        }
    }
}

impl TryFrom<String> for InterfaceVersion {
    type Error = InterfaceValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_from(value.as_str())
    }
}

impl Serialize for InterfaceVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(CURRENT_INTERFACE_VERSION)
    }
}

impl<'de> Deserialize<'de> for InterfaceVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::try_from(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CorrelationId(Uuid);

impl CorrelationId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }
}

impl Default for CorrelationId {
    fn default() -> Self {
        Self::new()
    }
}

/// A caller-provided key that can safely identify a repeated request.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for IdempotencyKey {
    type Error = InterfaceValidationError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        if !(1..=128).contains(&value.len())
            || !value
                .as_bytes()
                .iter()
                .all(|byte| (0x20..=0x7e).contains(byte))
        {
            return Err(InterfaceValidationError::InvalidIdempotencyKey);
        }
        Ok(Self(value.to_owned()))
    }
}

impl TryFrom<String> for IdempotencyKey {
    type Error = InterfaceValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_from(value.as_str())
    }
}

impl Serialize for IdempotencyKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for IdempotencyKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::try_from(value).map_err(serde::de::Error::custom)
    }
}

#[derive(
    Debug, Clone, Copy, Default, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct EventCursor(pub u64);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestContext {
    pub interface_version: InterfaceVersion,
    pub correlation_id: CorrelationId,
    pub principal_id: PrincipalId,
    pub capabilities: CapabilitySet,
    pub deadline: DateTime<Utc>,
    pub idempotency_key: Option<IdempotencyKey>,
}

impl RequestContext {
    pub fn new_for_test(
        principal_id: PrincipalId,
        capabilities: impl IntoIterator<Item = Capability>,
        deadline: DateTime<Utc>,
    ) -> Self {
        Self {
            interface_version: InterfaceVersion::CURRENT,
            correlation_id: CorrelationId::new(),
            principal_id,
            capabilities: CapabilitySet::new(capabilities),
            deadline,
            idempotency_key: None,
        }
    }

    /// Checks values whose validity depends on the instant an adapter dispatches the request.
    pub fn validate_at(
        &self,
        dispatch_time: DateTime<Utc>,
    ) -> Result<(), InterfaceValidationError> {
        if self.deadline <= dispatch_time {
            return Err(InterfaceValidationError::ExpiredDeadline);
        }
        Ok(())
    }
}

/// The complete transport-neutral operation list. Each variant must declare authority below.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum InterfaceOperation {
    RuntimeInfo,
    CreateSession,
    ReadSession,
    DeleteSession,
    OpenPage,
    ReadPage,
    ClosePage,
    SubmitCommand,
    CreateCheckpoint,
    ReadCheckpoint,
    RecoverWorkflow,
    ReadArtifact,
    CaptureArtifact,
    SubscribeEvents,
}

impl InterfaceOperation {
    pub const fn required(self) -> &'static [Capability] {
        match self {
            Self::RuntimeInfo => &[Capability::SessionRead],
            Self::CreateSession => &[Capability::SessionWrite],
            Self::ReadSession => &[Capability::SessionRead],
            Self::DeleteSession => &[Capability::SessionWrite],
            Self::OpenPage => &[Capability::PageWrite],
            Self::ReadPage => &[Capability::PageRead],
            Self::ClosePage => &[Capability::PageWrite],
            Self::SubmitCommand => &[Capability::BrowserMutate],
            Self::CreateCheckpoint => &[Capability::RecoveryWrite],
            Self::ReadCheckpoint => &[Capability::RecoveryRead],
            Self::RecoverWorkflow => &[Capability::RecoveryWrite],
            Self::ReadArtifact => &[Capability::ArtifactRead],
            Self::CaptureArtifact => &[Capability::ArtifactCapture],
            Self::SubscribeEvents => &[Capability::SessionRead],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum InterfaceEvent {
    CommandOutcome {
        cursor: EventCursor,
        command_id: CommandId,
        outcome: CommandOutcome,
    },
    ArtifactCaptured {
        cursor: EventCursor,
        artifact_id: ArtifactId,
    },
    EventGap {
        earliest_available_cursor: EventCursor,
    },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InterfaceErrorCode {
    InvalidRequest,
    UnsupportedInterfaceVersion,
    InvalidIdempotencyKey,
    IdempotencyConflict,
    DeadlineExceeded,
    AuthenticationFailed,
    TokenExpired,
    MissingCapability,
    MalformedScope,
    UnsupportedOperation,
    NotFound,
    ResourceExhausted,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InterfaceError {
    pub code: InterfaceErrorCode,
    pub layer: ErrorLayer,
    pub message: String,
    pub correlation_id: CorrelationId,
    pub command_id: Option<CommandId>,
    pub retryable: bool,
    pub retry_after_ms: Option<u64>,
    pub reconciliation_required: bool,
    pub required_capability: Option<Capability>,
}

#[derive(Debug, Clone, Error, Eq, PartialEq)]
pub enum InterfaceValidationError {
    #[error("unsupported interface version: {0}")]
    UnsupportedInterfaceVersion(String),
    #[error("idempotency key must contain 1-128 printable ASCII characters")]
    InvalidIdempotencyKey,
    #[error("request deadline must be after dispatch time")]
    ExpiredDeadline,
}
