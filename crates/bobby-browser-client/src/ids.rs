//! Typed identifiers used across the `/v1` wire contract.

use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
use thiserror::Error;
use uuid::Uuid;

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
        #[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

uuid_id!(SessionId);
uuid_id!(PageId);
uuid_id!(CommandId);
uuid_id!(WorkflowId);
uuid_id!(AttemptId);
uuid_id!(EvidenceId);
uuid_id!(WorkerId);
uuid_id!(ArtifactId);
uuid_id!(CheckpointId);
uuid_id!(CompanionId);
uuid_id!(ProfileId);
uuid_id!(AttachmentId);

/// Runtime job identifier (`job_<uuid>`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct JobId(String);

impl JobId {
    pub fn new() -> Self {
        Self(format!("job_{}", Uuid::new_v4()))
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, JobIdError> {
        let value = value.into();
        let suffix = value.strip_prefix("job_").ok_or(JobIdError)?;
        Uuid::parse_str(suffix).map_err(|_| JobIdError)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for JobId {
    type Err = JobIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for JobId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, Error)]
#[error("job id must use the job_<uuid> runtime format")]
pub struct JobIdError;
