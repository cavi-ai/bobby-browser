use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeSet;
use std::iter::FromIterator;
use uuid::Uuid;

use crate::InterfaceOperation;

/// An authenticated caller. This identifier intentionally carries no credential material.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PrincipalId(Uuid);

impl PrincipalId {
    pub fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
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
    #[serde(rename = "artifact:read")]
    ArtifactRead,
    #[serde(rename = "artifact:capture")]
    ArtifactCapture,
    #[serde(rename = "recovery:read")]
    RecoveryRead,
    #[serde(rename = "recovery:write")]
    RecoveryWrite,
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
            Self::ArtifactRead => "artifact:read",
            Self::ArtifactCapture => "artifact:capture",
            Self::RecoveryRead => "recovery:read",
            Self::RecoveryWrite => "recovery:write",
        }
    }
}

/// A verified, canonicalized capability set. It serializes in lexical wire order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilitySet(BTreeSet<Capability>);

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
