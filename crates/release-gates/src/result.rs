use std::{io, path::Path};

use serde::{de::Error as _, ser::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[cfg(unix)]
use crate::persistence::{persist_bytes, AtomicPersistenceIo, OsAtomicPersistenceIo, OutputTarget};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateResult {
    schema_version: u32,
    pub suite: String,
    pub check: String,
    pub required: bool,
    pub status: GateStatus,
    pub duration_ms: u64,
    pub observations: Vec<GateObservation>,
    pub diagnostics: String,
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GateEnvelope<'a> {
    schema_version: u32,
    suite: &'a str,
    check: &'a str,
    required: bool,
    status: &'a GateStatus,
    duration_ms: u64,
    observations: &'a [GateObservation],
    diagnostics: &'a str,
    evidence_sha256: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GateEnvelopeOwned {
    schema_version: u32,
    suite: String,
    check: String,
    required: bool,
    status: GateStatus,
    duration_ms: u64,
    observations: Vec<GateObservation>,
    diagnostics: String,
    evidence_sha256: String,
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
    #[error("atomic gate result persistence is unsupported on this platform")]
    UnsupportedPlatform,
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
        Self {
            schema_version: RESULT_SCHEMA_VERSION,
            suite: suite.into(),
            check: check.into(),
            required,
            status,
            duration_ms,
            observations,
            diagnostics: String::new(),
        }
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn evidence_sha256(&self) -> Result<String, ResultError> {
        Ok(hex_digest(&self.canonical_evidence_bytes()?))
    }

    pub fn redact(&mut self, canaries: &[String]) {
        for canary in canaries.iter().filter(|canary| !canary.is_empty()) {
            self.suite = self.suite.replace(canary, "[REDACTED]");
            self.check = self.check.replace(canary, "[REDACTED]");
            for observation in &mut self.observations {
                observation.name = observation.name.replace(canary, "[REDACTED]");
                observation.value = observation.value.replace(canary, "[REDACTED]");
            }
            self.diagnostics = self.diagnostics.replace(canary, "[REDACTED]");
        }
    }

    pub fn digest_hex(&self) -> Result<String, ResultError> {
        Ok(hex_digest(&serde_json::to_vec(self)?))
    }

    #[cfg(unix)]
    pub fn write_json(&self, path: impl AsRef<Path>, max_bytes: usize) -> Result<(), ResultError> {
        self.write_json_with_io(path.as_ref(), max_bytes, &OsAtomicPersistenceIo)
    }

    #[cfg(not(unix))]
    pub fn write_json(&self, _: impl AsRef<Path>, _: usize) -> Result<(), ResultError> {
        Err(ResultError::UnsupportedPlatform)
    }

    #[cfg(unix)]
    fn write_json_with_io(
        &self,
        path: &Path,
        max_bytes: usize,
        persistence: &impl AtomicPersistenceIo,
    ) -> Result<(), ResultError> {
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > max_bytes {
            return Err(ResultError::TooLarge {
                actual_bytes: bytes.len(),
                max_bytes,
            });
        }

        let target = OutputTarget::open(path)?;
        persist_bytes(&target, &bytes, persistence)?;
        Ok(())
    }

    fn canonical_evidence_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&GateEvidence {
            schema_version: self.schema_version,
            suite: &self.suite,
            check: &self.check,
            required: self.required,
            status: &self.status,
            duration_ms: self.duration_ms,
            observations: &self.observations,
            diagnostics: &self.diagnostics,
        })
    }
}

impl Serialize for GateResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let evidence_sha256 = self
            .evidence_sha256()
            .map_err(|error| S::Error::custom(error.to_string()))?;
        GateEnvelope {
            schema_version: self.schema_version,
            suite: &self.suite,
            check: &self.check,
            required: self.required,
            status: &self.status,
            duration_ms: self.duration_ms,
            observations: &self.observations,
            diagnostics: &self.diagnostics,
            evidence_sha256: &evidence_sha256,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for GateResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let envelope = GateEnvelopeOwned::deserialize(deserializer)?;
        if envelope.schema_version != RESULT_SCHEMA_VERSION {
            return Err(D::Error::custom(format!(
                "unsupported gate result schema version {}; expected {}",
                envelope.schema_version, RESULT_SCHEMA_VERSION
            )));
        }

        let result = Self {
            schema_version: envelope.schema_version,
            suite: envelope.suite,
            check: envelope.check,
            required: envelope.required,
            status: envelope.status,
            duration_ms: envelope.duration_ms,
            observations: envelope.observations,
            diagnostics: envelope.diagnostics,
        };
        let expected = result
            .evidence_sha256()
            .map_err(|error| D::Error::custom(error.to_string()))?;
        if envelope.evidence_sha256 != expected {
            return Err(D::Error::custom(
                "evidenceSha256 does not match canonical evidence",
            ));
        }
        Ok(result)
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(all(test, unix))]
mod persistence_tests {
    use std::{
        ffi::OsStr,
        fs, io,
        path::Path,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::{GateResult, GateStatus};
    use crate::persistence::{
        AtomicPersistenceIo, FileIdentity, OsAtomicPersistenceIo, MAX_TEMPORARY_ATTEMPTS,
    };

    #[derive(Clone, Copy)]
    enum FailurePoint {
        Metadata,
        Write,
        FileSync,
        Rename,
    }

    struct InjectedFailureIo(FailurePoint);

    struct CollisionIo {
        collisions: usize,
        attempts: AtomicUsize,
    }

    struct ForeignReplacementIo;

    impl AtomicPersistenceIo for InjectedFailureIo {
        fn create(&self, directory: &fs::File, name: &OsStr) -> io::Result<fs::File> {
            OsAtomicPersistenceIo.create(directory, name)
        }

        fn metadata(&self, file: &fs::File) -> io::Result<FileIdentity> {
            if matches!(self.0, FailurePoint::Metadata) {
                return Err(io::Error::other("injected metadata"));
            }
            crate::persistence::file_identity(file)
        }

        fn write_all(&self, file: &mut fs::File, bytes: &[u8]) -> io::Result<()> {
            if matches!(self.0, FailurePoint::Write) {
                return Err(io::Error::other("injected write"));
            }
            std::io::Write::write_all(file, bytes)
        }

        fn sync_file(&self, file: &fs::File) -> io::Result<()> {
            if matches!(self.0, FailurePoint::FileSync) {
                return Err(io::Error::other("injected sync"));
            }
            file.sync_all()
        }

        fn rename(
            &self,
            directory: &fs::File,
            source: &OsStr,
            destination: &OsStr,
        ) -> io::Result<()> {
            if matches!(self.0, FailurePoint::Rename) {
                return Err(io::Error::other("injected rename"));
            }
            OsAtomicPersistenceIo.rename(directory, source, destination)
        }

        fn sync_directory(&self, directory: &fs::File) -> io::Result<()> {
            OsAtomicPersistenceIo.sync_directory(directory)
        }
    }

    impl AtomicPersistenceIo for CollisionIo {
        fn create(&self, directory: &fs::File, name: &OsStr) -> io::Result<fs::File> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt < self.collisions {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "injected name collision",
                ));
            }
            OsAtomicPersistenceIo.create(directory, name)
        }

        fn metadata(&self, file: &fs::File) -> io::Result<FileIdentity> {
            crate::persistence::file_identity(file)
        }

        fn write_all(&self, file: &mut fs::File, bytes: &[u8]) -> io::Result<()> {
            std::io::Write::write_all(file, bytes)
        }

        fn sync_file(&self, file: &fs::File) -> io::Result<()> {
            file.sync_all()
        }

        fn rename(
            &self,
            directory: &fs::File,
            source: &OsStr,
            destination: &OsStr,
        ) -> io::Result<()> {
            OsAtomicPersistenceIo.rename(directory, source, destination)
        }

        fn sync_directory(&self, directory: &fs::File) -> io::Result<()> {
            OsAtomicPersistenceIo.sync_directory(directory)
        }
    }

    impl AtomicPersistenceIo for ForeignReplacementIo {
        fn create(&self, directory: &fs::File, name: &OsStr) -> io::Result<fs::File> {
            OsAtomicPersistenceIo.create(directory, name)
        }

        fn metadata(&self, file: &fs::File) -> io::Result<FileIdentity> {
            crate::persistence::file_identity(file)
        }

        fn write_all(&self, file: &mut fs::File, bytes: &[u8]) -> io::Result<()> {
            std::io::Write::write_all(file, bytes)
        }

        fn sync_file(&self, file: &fs::File) -> io::Result<()> {
            file.sync_all()
        }

        fn rename(&self, directory: &fs::File, source: &OsStr, _: &OsStr) -> io::Result<()> {
            rustix::fs::unlinkat(directory, source, rustix::fs::AtFlags::empty())?;
            let mut replacement = OsAtomicPersistenceIo.create(directory, source)?;
            std::io::Write::write_all(&mut replacement, b"foreign replacement")?;
            Err(io::Error::other("injected rename after replacement"))
        }

        fn sync_directory(&self, directory: &fs::File) -> io::Result<()> {
            OsAtomicPersistenceIo.sync_directory(directory)
        }
    }

    fn gate_result() -> GateResult {
        GateResult::new("security", "check", true, GateStatus::Passed, 1, vec![])
    }

    fn owned_temporary_files(dir: &Path) -> Vec<std::path::PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.contains(".release-gates-") && name.ends_with(".tmp"))
            })
            .collect()
    }

    #[test]
    fn gate_result_cleans_owned_temporary_files_after_post_create_failures() {
        for (name, point) in [
            ("metadata", FailurePoint::Metadata),
            ("write", FailurePoint::Write),
            ("sync", FailurePoint::FileSync),
            ("rename", FailurePoint::Rename),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let output = dir.path().join(format!("result-{name}.json"));

            assert!(gate_result()
                .write_json_with_io(&output, 4096, &InjectedFailureIo(point))
                .is_err());
            assert!(!output.exists());
            assert!(owned_temporary_files(dir.path()).is_empty());

            gate_result().write_json(&output, 4096).unwrap();
            assert!(output.exists());
            assert!(owned_temporary_files(dir.path()).is_empty());
        }
    }

    #[test]
    fn gate_result_retries_collisions_and_bounds_temporary_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("result.json");
        let retrying = CollisionIo {
            collisions: 3,
            attempts: AtomicUsize::new(0),
        };

        gate_result()
            .write_json_with_io(&output, 4096, &retrying)
            .unwrap();
        assert_eq!(retrying.attempts.load(Ordering::SeqCst), 4);
        assert!(owned_temporary_files(dir.path()).is_empty());

        let exhausted_output = dir.path().join("exhausted.json");
        let exhausted = CollisionIo {
            collisions: usize::MAX,
            attempts: AtomicUsize::new(0),
        };
        let error = gate_result()
            .write_json_with_io(&exhausted_output, 4096, &exhausted)
            .unwrap_err();
        assert!(
            matches!(error, super::ResultError::Io(ref source) if source.kind() == io::ErrorKind::AlreadyExists)
        );
        assert_eq!(
            exhausted.attempts.load(Ordering::SeqCst),
            MAX_TEMPORARY_ATTEMPTS
        );
        assert!(!exhausted_output.exists());
        assert!(owned_temporary_files(dir.path()).is_empty());
    }

    #[test]
    fn gate_result_cleanup_never_removes_a_foreign_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("result.json");

        assert!(gate_result()
            .write_json_with_io(&output, 4096, &ForeignReplacementIo)
            .is_err());
        assert!(!output.exists());
        let temporary_files = owned_temporary_files(dir.path());
        assert_eq!(temporary_files.len(), 1);
        assert_eq!(
            fs::read(&temporary_files[0]).unwrap(),
            b"foreign replacement"
        );
    }
}
