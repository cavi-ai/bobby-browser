//! Authentication and capability types for the `/v1` interface.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeSet;
use std::iter::FromIterator;
use uuid::Uuid;

use crate::InterfaceOperation;

/// Authenticated caller id. Carries no credential material.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct PrincipalId(Uuid);

impl PrincipalId {
    pub fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

/// Capability granted by a token or policy.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    #[serde(rename = "session:read")]
    SessionRead,
    #[serde(rename = "session:write")]
    SessionWrite,
    #[serde(rename = "page:read")]
    PageRead,
    #[serde(rename = "page:write")]
    PageWrite,
    #[serde(rename = "browser:mutate")]
    BrowserMutate,
    #[serde(rename = "file:upload")]
    FileUpload,
    #[serde(rename = "file:download")]
    FileDownload,
    #[serde(rename = "javascript:evaluate")]
    JavascriptEvaluate,
    #[serde(rename = "intent:execute")]
    IntentExecute,
    #[serde(rename = "vision:assist")]
    VisionAssist,
    #[serde(rename = "artifact:read")]
    ArtifactRead,
    #[serde(rename = "context:read")]
    ContextRead,
    #[serde(rename = "artifact:capture")]
    ArtifactCapture,
    #[serde(rename = "recovery:read")]
    RecoveryRead,
    #[serde(rename = "recovery:write")]
    RecoveryWrite,
    #[serde(rename = "job:submit")]
    JobSubmit,
    #[serde(rename = "job:read")]
    JobRead,
    #[serde(rename = "job:cancel")]
    JobCancel,
    #[serde(rename = "authority:admin")]
    AuthorityAdmin,
    #[serde(rename = "browser:fingerprint")]
    BrowserFingerprint,
    #[serde(rename = "browser:humanize")]
    BrowserHumanize,
}

impl Capability {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionRead => "session:read",
            Self::SessionWrite => "session:write",
            Self::PageRead => "page:read",
            Self::PageWrite => "page:write",
            Self::BrowserMutate => "browser:mutate",
            Self::FileUpload => "file:upload",
            Self::FileDownload => "file:download",
            Self::JavascriptEvaluate => "javascript:evaluate",
            Self::IntentExecute => "intent:execute",
            Self::VisionAssist => "vision:assist",
            Self::ArtifactRead => "artifact:read",
            Self::ContextRead => "context:read",
            Self::ArtifactCapture => "artifact:capture",
            Self::RecoveryRead => "recovery:read",
            Self::RecoveryWrite => "recovery:write",
            Self::JobSubmit => "job:submit",
            Self::JobRead => "job:read",
            Self::JobCancel => "job:cancel",
            Self::AuthorityAdmin => "authority:admin",
            Self::BrowserFingerprint => "browser:fingerprint",
            Self::BrowserHumanize => "browser:humanize",
        }
    }
}

/// Canonical capability set. Serializes in lexical wire order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CapabilitySet(BTreeSet<Capability>);

impl std::str::FromStr for Capability {
    type Err = UnknownCapability;

    /// Parses the wire string. Sole parse table: bootstrap files, broker
    /// startup credentials, and every stdio gateway accept exactly these
    /// strings. Do not add a per-binary match; they drift and fail closed.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "session:read" => Self::SessionRead,
            "session:write" => Self::SessionWrite,
            "page:read" => Self::PageRead,
            "page:write" => Self::PageWrite,
            "browser:mutate" => Self::BrowserMutate,
            "file:upload" => Self::FileUpload,
            "file:download" => Self::FileDownload,
            "javascript:evaluate" => Self::JavascriptEvaluate,
            "intent:execute" => Self::IntentExecute,
            "vision:assist" => Self::VisionAssist,
            "artifact:read" => Self::ArtifactRead,
            "context:read" => Self::ContextRead,
            "artifact:capture" => Self::ArtifactCapture,
            "recovery:read" => Self::RecoveryRead,
            "recovery:write" => Self::RecoveryWrite,
            "job:submit" => Self::JobSubmit,
            "job:read" => Self::JobRead,
            "job:cancel" => Self::JobCancel,
            "authority:admin" => Self::AuthorityAdmin,
            "browser:fingerprint" => Self::BrowserFingerprint,
            "browser:humanize" => Self::BrowserHumanize,
            _ => return Err(UnknownCapability(value.to_owned())),
        })
    }
}

/// A capability wire string no variant claims.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown capability: {0}")]
pub struct UnknownCapability(pub String);

impl CapabilitySet {
    pub fn new(capabilities: impl IntoIterator<Item = Capability>) -> Self {
        capabilities.into_iter().collect()
    }

    pub fn contains(&self, capability: Capability) -> bool {
        self.0.contains(&capability)
    }

    pub fn allows(&self, operation: InterfaceOperation) -> bool {
        operation
            .required()
            .iter()
            .all(|capability| self.contains(*capability))
    }
}

impl FromIterator<Capability> for CapabilitySet {
    fn from_iter<T: IntoIterator<Item = Capability>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl Serialize for CapabilitySet {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut capabilities: Vec<_> = self.0.iter().copied().collect();
        capabilities.sort_by_key(|capability| capability.as_str());
        capabilities.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CapabilitySet {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let capabilities = Vec::<Capability>::deserialize(deserializer)?;
        let mut verified = BTreeSet::new();
        for capability in capabilities {
            if !verified.insert(capability) {
                return Err(serde::de::Error::custom("duplicate capability"));
            }
        }
        Ok(Self(verified))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_admin_serde_round_trip() {
        let json = serde_json::to_string(&Capability::AuthorityAdmin).unwrap();
        assert_eq!(json, "\"authority:admin\"");
        let parsed: Capability = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, Capability::AuthorityAdmin);
    }
}
