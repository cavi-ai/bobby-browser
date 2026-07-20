use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_CHECK_TIMEOUT_SECS: u64 = 900;
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReleaseManifest {
    pub schema_version: u32,
    pub security: SecurityManifest,
    pub secret_canaries: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SecurityManifest {
    pub required: bool,
    pub timeout_secs: u64,
    pub max_output_bytes: usize,
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("invalid release manifest: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("unsupported schema version {actual}; expected {expected}")]
    Version { actual: u32, expected: u32 },
    #[error("invalid release manifest: {0}")]
    Invalid(&'static str),
}

impl ReleaseManifest {
    pub fn from_slice(input: &[u8]) -> Result<Self, ManifestError> {
        let manifest: Self = serde_json::from_slice(input)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(ManifestError::Version {
                actual: self.schema_version,
                expected: MANIFEST_SCHEMA_VERSION,
            });
        }
        if self.security.timeout_secs == 0 {
            return Err(ManifestError::Invalid(
                "security.timeoutSecs must be positive",
            ));
        }
        if self.security.max_output_bytes == 0 {
            return Err(ManifestError::Invalid(
                "security.maxOutputBytes must be positive",
            ));
        }
        if self.secret_canaries.is_empty() {
            return Err(ManifestError::Invalid("secretCanaries must not be empty"));
        }
        if self.secret_canaries.iter().any(|value| value.is_empty()) {
            return Err(ManifestError::Invalid(
                "secretCanaries entries must not be empty",
            ));
        }
        Ok(())
    }
}
