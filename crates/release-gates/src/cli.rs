use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

#[cfg(unix)]
use std::{ffi::OsString, fs::File, io::Read, os::unix::fs::MetadataExt};

#[cfg(unix)]
use rustix::fs::{Mode, OFlags};

use serde::{de::Error as _, ser::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::security::security_catalog;
use crate::{
    evaluate, security_catalog_sha256, CertificationVerdict, GateResult, GateStatus, ManifestError,
    PolicyError, ProcessRunner, ReleaseManifest, SecurityGate,
};

#[cfg(unix)]
use crate::persistence::{
    open_directory, persist_bytes, relative_identity, validated_file_name,
    AtomicPersistenceIo as BundlePersistenceIo, FileIdentity,
    OsAtomicPersistenceIo as OsBundlePersistenceIo, OutputTarget,
};

#[cfg(all(test, unix))]
use crate::persistence::file_identity;

pub const BUNDLE_SCHEMA_VERSION: u32 = 1;
/// Maximum accepted manifest size. The manifest descriptor is checked against
/// this 64 KiB bound before any buffer allocation and again while reading.
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Security,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    pub command: Command,
    pub manifest: PathBuf,
    pub output: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliFailureStage {
    PreExecution,
    PostExecution,
}

#[derive(Debug, Error)]
pub enum CliError {
    #[error("usage: release-gates security --manifest <path> --output <path>")]
    Usage,
    #[error("duplicate option: {0}")]
    DuplicateOption(&'static str),
    #[error("unknown option: {0}")]
    UnknownOption(String),
    #[error("path for {option} must not contain '..': {path}")]
    ParentTraversal { option: &'static str, path: String },
    #[error("manifest and output must resolve to different files")]
    PathConflict,
    #[error("failed to resolve manifest and output paths: {0}")]
    PathResolution(#[source] io::Error),
    #[error("security certification persistence is unsupported on this platform")]
    UnsupportedPlatform,
    #[error("failed to read release manifest: {0}")]
    ReadManifest(#[source] io::Error),
    #[error("release manifest must be a regular file")]
    ManifestNotRegular,
    #[error("release manifest is {actual_bytes} bytes; exceeds {max_bytes} byte limit")]
    ManifestTooLarge { actual_bytes: u64, max_bytes: usize },
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error("failed to create output directory: {0}")]
    CreateOutputDirectory(#[source] io::Error),
    #[error(transparent)]
    Policy(#[from] PolicyError),
    #[error(transparent)]
    Bundle(#[from] BundleError),
}

impl CliError {
    pub const fn failure_stage(&self) -> CliFailureStage {
        match self {
            Self::Policy(_) | Self::Bundle(_) => CliFailureStage::PostExecution,
            Self::Usage
            | Self::DuplicateOption(_)
            | Self::UnknownOption(_)
            | Self::ParentTraversal { .. }
            | Self::PathConflict
            | Self::PathResolution(_)
            | Self::UnsupportedPlatform
            | Self::ReadManifest(_)
            | Self::ManifestNotRegular
            | Self::ManifestTooLarge { .. }
            | Self::Manifest(_)
            | Self::CreateOutputDirectory(_) => CliFailureStage::PreExecution,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificationBundle {
    schema_version: u32,
    pub catalog_sha256: String,
    pub manifest_sha256: String,
    pub results: Vec<GateResult>,
    pub verdict: CertificationVerdict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum BundleVerdict {
    Passed,
    Degraded,
    Blocked,
}

impl From<CertificationVerdict> for BundleVerdict {
    fn from(verdict: CertificationVerdict) -> Self {
        match verdict {
            CertificationVerdict::Passed => Self::Passed,
            CertificationVerdict::Degraded => Self::Degraded,
            CertificationVerdict::Blocked => Self::Blocked,
        }
    }
}

impl From<BundleVerdict> for CertificationVerdict {
    fn from(verdict: BundleVerdict) -> Self {
        match verdict {
            BundleVerdict::Passed => Self::Passed,
            BundleVerdict::Degraded => Self::Degraded,
            BundleVerdict::Blocked => Self::Blocked,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BundleEvidence<'a> {
    schema_version: u32,
    catalog_sha256: &'a str,
    manifest_sha256: &'a str,
    results: &'a [GateResult],
    verdict: BundleVerdict,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BundleEnvelope<'a> {
    schema_version: u32,
    catalog_sha256: &'a str,
    manifest_sha256: &'a str,
    results: &'a [GateResult],
    verdict: BundleVerdict,
    bundle_sha256: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BundleEnvelopeOwned {
    schema_version: u32,
    catalog_sha256: String,
    manifest_sha256: String,
    results: Vec<GateResult>,
    verdict: BundleVerdict,
    bundle_sha256: String,
}

#[derive(Debug, Error)]
pub enum BundleError {
    #[error("invalid certification bundle: {0}")]
    Invalid(String),
    #[error("failed to serialize certification bundle: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("certification bundle is {actual_bytes} bytes; exceeds {max_bytes} byte limit")]
    TooLarge {
        actual_bytes: usize,
        max_bytes: usize,
    },
    #[error("atomic certification persistence is unsupported on this platform")]
    UnsupportedPlatform,
    #[error("failed to persist certification bundle: {0}")]
    Io(#[from] io::Error),
}

impl CertificationBundle {
    pub fn try_new(
        manifest_sha256: String,
        results: Vec<GateResult>,
        verdict: CertificationVerdict,
    ) -> Result<Self, BundleError> {
        let bundle = Self {
            schema_version: BUNDLE_SCHEMA_VERSION,
            catalog_sha256: security_catalog_sha256(),
            manifest_sha256,
            results,
            verdict,
        };
        bundle.validate_semantics()?;
        Ok(bundle)
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn bundle_sha256(&self) -> Result<String, BundleError> {
        let bytes = serde_json::to_vec(&BundleEvidence {
            schema_version: self.schema_version,
            catalog_sha256: &self.catalog_sha256,
            manifest_sha256: &self.manifest_sha256,
            results: &self.results,
            verdict: self.verdict.into(),
        })?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }

    fn validate_semantics(&self) -> Result<(), BundleError> {
        if self.schema_version != BUNDLE_SCHEMA_VERSION {
            return Err(BundleError::Invalid(format!(
                "unsupported schema version {}; expected {BUNDLE_SCHEMA_VERSION}",
                self.schema_version
            )));
        }
        validate_sha256("manifestSha256", &self.manifest_sha256)?;
        validate_sha256("catalogSha256", &self.catalog_sha256)?;
        let compiled_catalog_sha256 = security_catalog_sha256();
        if self.catalog_sha256 != compiled_catalog_sha256 {
            return Err(BundleError::Invalid(
                "catalogSha256 does not match the compiled security catalog".into(),
            ));
        }

        let catalog = security_catalog();
        if self.results.len() != catalog.len() {
            return Err(BundleError::Invalid(format!(
                "security catalog requires exactly {} results; observed {}",
                catalog.len(),
                self.results.len()
            )));
        }
        let mut observed = std::collections::BTreeSet::new();
        for (index, (result, check)) in self.results.iter().zip(catalog).enumerate() {
            if !observed.insert((result.suite.as_str(), result.check.as_str())) {
                return Err(BundleError::Invalid(format!(
                    "duplicate result identity {}/{}",
                    result.suite, result.check
                )));
            }
            if result.suite != "security" || result.check != check.name {
                return Err(BundleError::Invalid(format!(
                    "catalog result {index} must be security/{}; observed {}/{}",
                    check.name, result.suite, result.check
                )));
            }
            if check.required && !result.required {
                return Err(BundleError::Invalid(format!(
                    "required catalog result security/{} is not marked required",
                    check.name
                )));
            }
        }

        let recomputed = evaluate(&["security"], &self.results)
            .map_err(|error| BundleError::Invalid(format!("policy evaluation failed: {error}")))?;
        if self.verdict != recomputed {
            return Err(BundleError::Invalid(format!(
                "stored verdict {:?} does not match recomputed verdict {recomputed:?}",
                self.verdict
            )));
        }
        Ok(())
    }

    #[cfg(unix)]
    pub fn write_json(&self, path: impl AsRef<Path>, max_bytes: usize) -> Result<(), BundleError> {
        self.write_json_with_io(path.as_ref(), max_bytes, &OsBundlePersistenceIo)
    }

    #[cfg(not(unix))]
    pub fn write_json(&self, _: impl AsRef<Path>, _: usize) -> Result<(), BundleError> {
        Err(BundleError::UnsupportedPlatform)
    }

    #[cfg(unix)]
    fn write_json_with_io(
        &self,
        path: &Path,
        max_bytes: usize,
        persistence: &impl BundlePersistenceIo,
    ) -> Result<(), BundleError> {
        let target = OutputTarget::open(path)?;
        self.write_json_to_target_with_io(&target, max_bytes, persistence)
    }

    #[cfg(unix)]
    fn write_json_to_target_with_io(
        &self,
        target: &OutputTarget,
        max_bytes: usize,
        persistence: &impl BundlePersistenceIo,
    ) -> Result<(), BundleError> {
        self.validate_semantics()?;
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > max_bytes {
            return Err(BundleError::TooLarge {
                actual_bytes: bytes.len(),
                max_bytes,
            });
        }

        persist_bytes(target, &bytes, persistence)?;
        Ok(())
    }
}

fn validate_sha256(name: &str, value: &str) -> Result<(), BundleError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(BundleError::Invalid(format!(
            "{name} must be 64 lowercase hexadecimal characters"
        )));
    }
    Ok(())
}

impl Serialize for CertificationBundle {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate_semantics()
            .map_err(|error| S::Error::custom(error.to_string()))?;
        let bundle_sha256 = self
            .bundle_sha256()
            .map_err(|error| S::Error::custom(error.to_string()))?;
        BundleEnvelope {
            schema_version: self.schema_version,
            catalog_sha256: &self.catalog_sha256,
            manifest_sha256: &self.manifest_sha256,
            results: &self.results,
            verdict: self.verdict.into(),
            bundle_sha256: &bundle_sha256,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CertificationBundle {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let envelope = BundleEnvelopeOwned::deserialize(deserializer)?;
        let bundle = Self {
            schema_version: envelope.schema_version,
            catalog_sha256: envelope.catalog_sha256,
            manifest_sha256: envelope.manifest_sha256,
            results: envelope.results,
            verdict: envelope.verdict.into(),
        };
        bundle
            .validate_semantics()
            .map_err(|error| D::Error::custom(error.to_string()))?;
        let expected = bundle
            .bundle_sha256()
            .map_err(|error| D::Error::custom(error.to_string()))?;
        if envelope.bundle_sha256 != expected {
            return Err(D::Error::custom(
                "bundleSha256 does not match canonical bundle evidence",
            ));
        }
        Ok(bundle)
    }
}

pub fn parse_args<I, S>(args: I) -> Result<Cli, CliError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut args = args.into_iter().map(Into::into);
    if args.next().as_deref() != Some("security") {
        return Err(CliError::Usage);
    }

    let mut manifest = None;
    let mut output = None;
    while let Some(flag) = args.next() {
        let destination = match flag.as_str() {
            "--manifest" => &mut manifest,
            "--output" => &mut output,
            value if value.starts_with('-') => return Err(CliError::UnknownOption(value.into())),
            _ => return Err(CliError::Usage),
        };
        if destination.is_some() {
            return Err(CliError::DuplicateOption(match flag.as_str() {
                "--manifest" => "--manifest",
                _ => "--output",
            }));
        }
        let value = args.next().ok_or(CliError::Usage)?;
        if value.starts_with('-') || value.is_empty() {
            return Err(CliError::Usage);
        }
        *destination = Some(PathBuf::from(value));
    }

    let manifest = manifest.ok_or(CliError::Usage)?;
    let output = output.ok_or(CliError::Usage)?;
    validate_path("--manifest", &manifest)?;
    validate_path("--output", &output)?;

    Ok(Cli {
        command: Command::Security,
        manifest,
        output,
    })
}

pub const fn exit_code(verdict: CertificationVerdict) -> i32 {
    match verdict {
        CertificationVerdict::Passed => 0,
        CertificationVerdict::Degraded => 3,
        CertificationVerdict::Blocked => 1,
    }
}

pub const fn failure_exit_code(error: &CliError) -> i32 {
    match error.failure_stage() {
        CliFailureStage::PreExecution => 2,
        CliFailureStage::PostExecution => 1,
    }
}

struct ValidatedPaths {
    #[cfg(unix)]
    manifest: OpenedManifest,
    output: PathBuf,
}

#[cfg(unix)]
struct OpenedManifest {
    file: File,
    identity: FileIdentity,
    size_hint: usize,
}

#[cfg(unix)]
impl OpenedManifest {
    fn open(path: &Path) -> Result<Self, CliError> {
        let fd = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|error| CliError::ReadManifest(error.into()))?;
        let file = File::from(fd);
        let metadata = file.metadata().map_err(CliError::ReadManifest)?;
        if !metadata.file_type().is_file() {
            return Err(CliError::ManifestNotRegular);
        }
        if metadata.len() > MAX_MANIFEST_BYTES as u64 {
            return Err(CliError::ManifestTooLarge {
                actual_bytes: metadata.len(),
                max_bytes: MAX_MANIFEST_BYTES,
            });
        }
        Ok(Self {
            identity: FileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            size_hint: metadata.len() as usize,
            file,
        })
    }

    fn read_bounded(&mut self) -> Result<Vec<u8>, CliError> {
        let mut bytes = Vec::with_capacity(self.size_hint);
        (&mut self.file)
            .take((MAX_MANIFEST_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(CliError::ReadManifest)?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(CliError::ManifestTooLarge {
                actual_bytes: bytes.len() as u64,
                max_bytes: MAX_MANIFEST_BYTES,
            });
        }
        Ok(bytes)
    }
}

#[cfg(unix)]
fn validate_distinct_paths(cli: &Cli, repo_root: &Path) -> Result<ValidatedPaths, CliError> {
    let canonical_root = fs::canonicalize(repo_root).map_err(CliError::PathResolution)?;
    let manifest_path = absolute_path(&cli.manifest, &canonical_root);
    let manifest = OpenedManifest::open(&manifest_path)?;
    let output_path = absolute_path(&cli.output, &canonical_root);
    let canonical_output =
        canonicalize_allow_missing(&output_path).map_err(CliError::PathResolution)?;
    match fs::metadata(&canonical_output) {
        Ok(output_metadata)
            if manifest.identity
                == (FileIdentity {
                    device: output_metadata.dev(),
                    inode: output_metadata.ino(),
                }) =>
        {
            return Err(CliError::PathConflict)
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(CliError::PathResolution(error)),
    }

    Ok(ValidatedPaths {
        manifest,
        output: canonical_output,
    })
}

#[cfg(not(unix))]
fn validate_distinct_paths(_: &Cli, _: &Path) -> Result<ValidatedPaths, CliError> {
    Err(CliError::UnsupportedPlatform)
}

#[cfg(unix)]
fn absolute_path(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    }
}

#[cfg(unix)]
fn canonicalize_allow_missing(path: &Path) -> io::Result<PathBuf> {
    let mut existing = path.to_owned();
    let mut missing = Vec::<OsString>::new();

    loop {
        match fs::symlink_metadata(&existing) {
            Ok(_) => {
                let mut canonical = fs::canonicalize(&existing)?;
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let component = existing.file_name().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "output path has no existing ancestor",
                    )
                })?;
                missing.push(component.to_owned());
                existing = existing
                    .parent()
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::NotFound,
                            "output path has no existing ancestor",
                        )
                    })?
                    .to_owned();
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(unix)]
fn prepare_output_target(paths: &ValidatedPaths) -> Result<OutputTarget, CliError> {
    let parent = paths
        .output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            CliError::PathResolution(io::Error::new(
                io::ErrorKind::InvalidInput,
                "missing output parent directory",
            ))
        })?;
    fs::create_dir_all(parent).map_err(CliError::CreateOutputDirectory)?;
    let canonical_parent = fs::canonicalize(parent).map_err(CliError::PathResolution)?;
    let target = OutputTarget {
        directory: open_directory(&canonical_parent).map_err(CliError::PathResolution)?,
        file_name: validated_file_name(&paths.output).map_err(CliError::PathResolution)?,
    };

    match relative_identity(&target.directory, &target.file_name, false) {
        Ok(identity) if identity == paths.manifest.identity => Err(CliError::PathConflict),
        Ok(_) => Ok(target),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(target),
        Err(error) => Err(CliError::PathResolution(error)),
    }
}

pub async fn run_security<R>(
    cli: &Cli,
    repo_root: &Path,
    gate: &SecurityGate<R>,
) -> Result<CertificationBundle, CliError>
where
    R: ProcessRunner,
{
    let mut paths = validate_distinct_paths(cli, repo_root)?;
    #[cfg(unix)]
    let manifest_bytes = paths.manifest.read_bounded()?;
    #[cfg(not(unix))]
    return Err(CliError::UnsupportedPlatform);
    let manifest = ReleaseManifest::from_slice(&manifest_bytes)?;
    let manifest_sha256 = format!("{:x}", Sha256::digest(&manifest_bytes));
    #[cfg(unix)]
    let output_target = prepare_output_target(&paths)?;

    let results = gate.run(repo_root, &manifest).await;
    let verdict = evaluate(&["security"], &results)?;
    let bundle = CertificationBundle::try_new(manifest_sha256, results, verdict)?;
    #[cfg(unix)]
    bundle.write_json_to_target_with_io(
        &output_target,
        manifest.security.max_output_bytes,
        &OsBundlePersistenceIo,
    )?;
    #[cfg(not(unix))]
    return Err(CliError::UnsupportedPlatform);
    Ok(bundle)
}

pub fn summary_lines(bundle: &CertificationBundle) -> Vec<String> {
    let mut lines = bundle
        .results
        .iter()
        .map(|result| {
            format!(
                "{}/{}: {}",
                result.suite,
                result.check,
                status_name(&result.status)
            )
        })
        .collect::<Vec<_>>();
    lines.push(format!("release verdict: {}", verdict_name(bundle.verdict)));
    lines
}

fn status_name(status: &GateStatus) -> &'static str {
    match status {
        GateStatus::Passed => "passed",
        GateStatus::Degraded => "degraded",
        GateStatus::Blocked => "blocked",
    }
}

fn verdict_name(verdict: CertificationVerdict) -> &'static str {
    match verdict {
        CertificationVerdict::Passed => "passed",
        CertificationVerdict::Degraded => "degraded",
        CertificationVerdict::Blocked => "blocked",
    }
}

fn validate_path(option: &'static str, path: &Path) -> Result<(), CliError> {
    if path.components().any(|part| part == Component::ParentDir) {
        return Err(CliError::ParentTraversal {
            option,
            path: path.display().to_string(),
        });
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod manifest_descriptor_tests {
    use super::OpenedManifest;

    #[test]
    fn opened_manifest_descriptor_survives_path_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        let moved = dir.path().join("opened-manifest.json");
        let original = br#"{"schemaVersion":1}"#.to_vec();
        std::fs::write(&path, &original).unwrap();

        let mut opened = OpenedManifest::open(&path).unwrap();
        std::fs::rename(&path, &moved).unwrap();
        std::fs::write(&path, br#"{"schemaVersion":999}"#).unwrap();

        assert_eq!(opened.read_bounded().unwrap(), original);
    }
}

#[cfg(all(test, unix))]
mod persistence_tests {
    use std::{ffi::OsStr, fs, io, path::Path};

    use super::{BundlePersistenceIo, CertificationBundle, FileIdentity};
    use crate::{CertificationVerdict, GateResult, GateStatus};

    #[derive(Clone, Copy)]
    enum FailurePoint {
        Metadata,
        Write,
        FileSync,
        Rename,
    }

    struct InjectedFailureIo(FailurePoint);

    struct ForeignReplacementIo;

    impl BundlePersistenceIo for InjectedFailureIo {
        fn create(&self, directory: &fs::File, name: &OsStr) -> io::Result<fs::File> {
            super::OsBundlePersistenceIo.create(directory, name)
        }

        fn metadata(&self, file: &fs::File) -> io::Result<FileIdentity> {
            if matches!(self.0, FailurePoint::Metadata) {
                return Err(io::Error::other("injected metadata"));
            }
            super::file_identity(file)
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
            super::OsBundlePersistenceIo.rename(directory, source, destination)
        }

        fn sync_directory(&self, directory: &fs::File) -> io::Result<()> {
            super::OsBundlePersistenceIo.sync_directory(directory)
        }
    }

    impl BundlePersistenceIo for ForeignReplacementIo {
        fn create(&self, directory: &fs::File, name: &OsStr) -> io::Result<fs::File> {
            super::OsBundlePersistenceIo.create(directory, name)
        }

        fn metadata(&self, file: &fs::File) -> io::Result<FileIdentity> {
            super::file_identity(file)
        }

        fn write_all(&self, file: &mut fs::File, bytes: &[u8]) -> io::Result<()> {
            std::io::Write::write_all(file, bytes)
        }

        fn sync_file(&self, file: &fs::File) -> io::Result<()> {
            file.sync_all()
        }

        fn rename(&self, directory: &fs::File, source: &OsStr, _: &OsStr) -> io::Result<()> {
            rustix::fs::unlinkat(directory, source, rustix::fs::AtFlags::empty())?;
            let mut replacement = super::OsBundlePersistenceIo.create(directory, source)?;
            std::io::Write::write_all(&mut replacement, b"foreign replacement")?;
            Err(io::Error::other("injected rename after replacement"))
        }

        fn sync_directory(&self, directory: &fs::File) -> io::Result<()> {
            super::OsBundlePersistenceIo.sync_directory(directory)
        }
    }

    fn bundle() -> CertificationBundle {
        CertificationBundle::try_new(
            "0".repeat(64),
            super::security_catalog()
                .iter()
                .map(|check| {
                    GateResult::new("security", check.name, true, GateStatus::Passed, 1, vec![])
                })
                .collect(),
            CertificationVerdict::Passed,
        )
        .unwrap()
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
    fn owned_temporary_file_is_cleaned_after_every_post_create_failure_and_retry_succeeds() {
        for (name, point) in [
            ("metadata", FailurePoint::Metadata),
            ("write", FailurePoint::Write),
            ("sync", FailurePoint::FileSync),
            ("rename", FailurePoint::Rename),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let output = dir.path().join(format!("security-{name}.json"));

            assert!(bundle()
                .write_json_with_io(&output, 4096, &InjectedFailureIo(point))
                .is_err());
            assert!(!output.exists());
            assert!(owned_temporary_files(dir.path()).is_empty());

            bundle().write_json(&output, 4096).unwrap();
            assert!(output.exists());
            assert!(owned_temporary_files(dir.path()).is_empty());
        }
    }

    #[test]
    fn cleanup_guard_never_removes_a_foreign_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("security.json");

        assert!(bundle()
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
