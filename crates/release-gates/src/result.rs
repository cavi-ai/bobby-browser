use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::Path,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const RESULT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum GateStatus {
    Passed,
    Degraded,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GateObservation {
    pub name: String,
    pub value: String,
}

impl GateObservation {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GateResult {
    pub schema_version: u32,
    pub suite: String,
    pub check: String,
    pub required: bool,
    pub status: GateStatus,
    pub duration_ms: u64,
    pub observations: Vec<GateObservation>,
    pub diagnostics: String,
    pub evidence_sha256: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GateEvidence<'a> {
    schema_version: u32,
    suite: &'a str,
    check: &'a str,
    required: bool,
    status: &'a GateStatus,
    duration_ms: u64,
    observations: &'a [GateObservation],
    diagnostics: &'a str,
}

#[derive(Debug, Error)]
pub enum ResultError {
    #[error("failed to serialize gate result: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("gate result is {actual_bytes} bytes; exceeds {max_bytes} byte limit")]
    TooLarge {
        actual_bytes: usize,
        max_bytes: usize,
    },
    #[error("failed to persist gate result: {0}")]
    Io(#[from] io::Error),
}

impl GateResult {
    pub fn new(
        suite: impl Into<String>,
        check: impl Into<String>,
        required: bool,
        status: GateStatus,
        duration_ms: u64,
        observations: Vec<GateObservation>,
    ) -> Self {
        let mut result = Self {
            schema_version: RESULT_SCHEMA_VERSION,
            suite: suite.into(),
            check: check.into(),
            required,
            status,
            duration_ms,
            observations,
            diagnostics: String::new(),
            evidence_sha256: String::new(),
        };
        result.recompute_evidence_sha256();
        result
    }

    pub fn redact(&mut self, canaries: &[String]) {
        for canary in canaries.iter().filter(|canary| !canary.is_empty()) {
            for observation in &mut self.observations {
                observation.name = observation.name.replace(canary, "[REDACTED]");
                observation.value = observation.value.replace(canary, "[REDACTED]");
            }
            self.diagnostics = self.diagnostics.replace(canary, "[REDACTED]");
        }
        self.recompute_evidence_sha256();
    }

    pub fn digest_hex(&self) -> String {
        hex_digest(&serde_json::to_vec(self).expect("GateResult serialization must succeed"))
    }

    pub fn write_json(&self, path: impl AsRef<Path>, max_bytes: usize) -> Result<(), ResultError> {
        let path = path.as_ref();
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > max_bytes {
            return Err(ResultError::TooLarge {
                actual_bytes: bytes.len(),
                max_bytes,
            });
        }

        let temporary_path = path.with_extension("json.tmp");
        let mut temporary_file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary_path)?;
        temporary_file.write_all(&bytes)?;
        temporary_file.sync_all()?;
        fs::rename(&temporary_path, path)?;
        sync_parent_directory(path)?;
        Ok(())
    }

    fn recompute_evidence_sha256(&mut self) {
        self.evidence_sha256 = hex_digest(
            &serde_json::to_vec(&GateEvidence {
                schema_version: self.schema_version,
                suite: &self.suite,
                check: &self.check,
                required: self.required,
                status: &self.status,
                duration_ms: self.duration_ms,
                observations: &self.observations,
                diagnostics: &self.diagnostics,
            })
            .expect("gate evidence serialization must succeed"),
        );
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_: &Path) -> io::Result<()> {
    Ok(())
}
