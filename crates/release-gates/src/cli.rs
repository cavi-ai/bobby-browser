use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

#[cfg(unix)]
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::Write,
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(unix)]
use rustix::fs::{AtFlags, Mode, OFlags};

use serde::{de::Error as _, ser::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    evaluate, CertificationVerdict, GateResult, GateStatus, ManifestError, PolicyError,
    ProcessRunner, ReleaseManifest, SecurityGate,
};

pub const BUNDLE_SCHEMA_VERSION: u32 = 1;

#[cfg(unix)]
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error("failed to create output directory: {0}")]
    CreateOutputDirectory(#[source] io::Error),
    #[error(transparent)]
    Policy(#[from] PolicyError),
    #[error(transparent)]
    Bundle(#[from] BundleError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificationBundle {
    schema_version: u32,
    pub manifest_sha256: String,
    pub results: Vec<GateResult>,
    pub verdict: CertificationVerdict,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
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
    manifest_sha256: &'a str,
    results: &'a [GateResult],
    verdict: BundleVerdict,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BundleEnvelope<'a> {
    schema_version: u32,
    manifest_sha256: &'a str,
    results: &'a [GateResult],
    verdict: BundleVerdict,
    bundle_sha256: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BundleEnvelopeOwned {
    schema_version: u32,
    manifest_sha256: String,
    results: Vec<GateResult>,
    verdict: BundleVerdict,
    bundle_sha256: String,
}

#[derive(Debug, Error)]
pub enum BundleError {
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
    pub fn new(
        manifest_sha256: String,
        results: Vec<GateResult>,
        verdict: CertificationVerdict,
    ) -> Self {
        Self {
            schema_version: BUNDLE_SCHEMA_VERSION,
            manifest_sha256,
            results,
            verdict,
        }
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn bundle_sha256(&self) -> Result<String, BundleError> {
        let bytes = serde_json::to_vec(&BundleEvidence {
            schema_version: self.schema_version,
            manifest_sha256: &self.manifest_sha256,
            results: &self.results,
            verdict: self.verdict.into(),
        })?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
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
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > max_bytes {
            return Err(BundleError::TooLarge {
                actual_bytes: bytes.len(),
                max_bytes,
            });
        }

        let mut temporary = OwnedTemporaryFile::create(target, persistence)?;
        persistence.write_all(&mut temporary.file, &bytes)?;
        persistence.sync_file(&temporary.file)?;
        persistence.rename(&target.directory, &temporary.name, &target.file_name)?;
        temporary.disarm();
        persistence.sync_directory(&target.directory)?;
        Ok(())
    }
}

#[cfg(unix)]
trait BundlePersistenceIo {
    fn create(&self, directory: &File, name: &OsStr) -> io::Result<File>;
    fn metadata(&self, file: &File) -> io::Result<FileIdentity>;
    fn write_all(&self, file: &mut File, bytes: &[u8]) -> io::Result<()>;
    fn sync_file(&self, file: &File) -> io::Result<()>;
    fn rename(&self, directory: &File, source: &OsStr, destination: &OsStr) -> io::Result<()>;
    fn sync_directory(&self, directory: &File) -> io::Result<()>;
}

#[cfg(unix)]
struct OsBundlePersistenceIo;

#[cfg(unix)]
impl BundlePersistenceIo for OsBundlePersistenceIo {
    fn create(&self, directory: &File, name: &OsStr) -> io::Result<File> {
        let fd = rustix::fs::openat(
            directory,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )?;
        Ok(File::from(fd))
    }

    fn metadata(&self, file: &File) -> io::Result<FileIdentity> {
        file_identity(file)
    }

    fn write_all(&self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
        file.write_all(bytes)
    }

    fn sync_file(&self, file: &File) -> io::Result<()> {
        rustix::fs::fsync(file).map_err(Into::into)
    }

    fn rename(&self, directory: &File, source: &OsStr, destination: &OsStr) -> io::Result<()> {
        rustix::fs::renameat(directory, source, directory, destination).map_err(Into::into)
    }

    fn sync_directory(&self, directory: &File) -> io::Result<()> {
        rustix::fs::fsync(directory).map_err(Into::into)
    }
}

#[cfg(unix)]
struct OutputTarget {
    directory: File,
    file_name: OsString,
}

#[cfg(unix)]
impl OutputTarget {
    fn open(path: &Path) -> io::Result<Self> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let file_name = validated_file_name(path)?;
        let canonical_parent = fs::canonicalize(parent)?;
        Ok(Self {
            directory: open_directory(&canonical_parent)?,
            file_name,
        })
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
struct OwnedTemporaryFile<'a> {
    directory: &'a File,
    name: OsString,
    file: File,
    identity: Option<FileIdentity>,
    armed: bool,
}

#[cfg(unix)]
impl<'a> OwnedTemporaryFile<'a> {
    fn create(
        target: &'a OutputTarget,
        persistence: &impl BundlePersistenceIo,
    ) -> io::Result<Self> {
        for _ in 0..128 {
            let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let mut temporary_name = OsString::from(".");
            temporary_name.push(&target.file_name);
            temporary_name.push(format!(
                ".release-gates-{}-{sequence}.tmp",
                std::process::id()
            ));
            match persistence.create(&target.directory, &temporary_name) {
                Ok(file) => {
                    // Ownership begins immediately after openat succeeds. If
                    // the following identity probe fails, Drop still compares
                    // the open fd to the directory entry before cleanup.
                    let mut temporary = Self {
                        directory: &target.directory,
                        name: temporary_name,
                        file,
                        identity: None,
                        armed: true,
                    };
                    temporary.identity = Some(persistence.metadata(&temporary.file)?);
                    return Ok(temporary);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }

        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique certification temporary file",
        ))
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(unix)]
impl Drop for OwnedTemporaryFile<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let expected = self.identity.or_else(|| file_identity(&self.file).ok());
        let actual = relative_identity(self.directory, &self.name, true).ok();
        let owned = expected.is_some() && expected == actual;
        if owned {
            let _ = rustix::fs::unlinkat(self.directory, &self.name, AtFlags::empty());
        }
    }
}

#[cfg(unix)]
fn validated_file_name(path: &Path) -> io::Result<OsString> {
    path.file_name()
        .map(OsStr::to_owned)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing output filename"))
}

#[cfg(unix)]
fn open_directory(path: &Path) -> io::Result<File> {
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )?;
    Ok(File::from(fd))
}

#[cfg(unix)]
fn file_identity(file: &File) -> io::Result<FileIdentity> {
    let stat = rustix::fs::fstat(file)?;
    Ok(FileIdentity {
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
    })
}

#[cfg(unix)]
fn relative_identity(directory: &File, name: &OsStr, no_follow: bool) -> io::Result<FileIdentity> {
    let flags = if no_follow {
        AtFlags::SYMLINK_NOFOLLOW
    } else {
        AtFlags::empty()
    };
    let stat = rustix::fs::statat(directory, name, flags)?;
    Ok(FileIdentity {
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
    })
}

impl Serialize for CertificationBundle {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let bundle_sha256 = self
            .bundle_sha256()
            .map_err(|error| S::Error::custom(error.to_string()))?;
        BundleEnvelope {
            schema_version: self.schema_version,
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
        if envelope.schema_version != BUNDLE_SCHEMA_VERSION {
            return Err(D::Error::custom(format!(
                "unsupported certification bundle schema version {}; expected {}",
                envelope.schema_version, BUNDLE_SCHEMA_VERSION
            )));
        }
        let bundle = Self {
            schema_version: envelope.schema_version,
            manifest_sha256: envelope.manifest_sha256,
            results: envelope.results,
            verdict: envelope.verdict.into(),
        };
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

struct ValidatedPaths {
    manifest: PathBuf,
    output: PathBuf,
    #[cfg(unix)]
    manifest_identity: FileIdentity,
}

#[cfg(unix)]
fn validate_distinct_paths(cli: &Cli, repo_root: &Path) -> Result<ValidatedPaths, CliError> {
    use std::os::unix::fs::MetadataExt;

    let canonical_root = fs::canonicalize(repo_root).map_err(CliError::PathResolution)?;
    let manifest_path = absolute_path(&cli.manifest, &canonical_root);
    let output_path = absolute_path(&cli.output, &canonical_root);
    if manifest_path == output_path {
        return Err(CliError::PathConflict);
    }
    let canonical_manifest = fs::canonicalize(&manifest_path).map_err(CliError::PathResolution)?;
    let canonical_output =
        canonicalize_allow_missing(&output_path).map_err(CliError::PathResolution)?;

    if canonical_manifest == canonical_output {
        return Err(CliError::PathConflict);
    }

    let manifest_metadata = fs::metadata(&canonical_manifest).map_err(CliError::PathResolution)?;
    match fs::metadata(&output_path) {
        Ok(output_metadata)
            if manifest_metadata.dev() == output_metadata.dev()
                && manifest_metadata.ino() == output_metadata.ino() =>
        {
            return Err(CliError::PathConflict);
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(CliError::PathResolution(error)),
    }

    Ok(ValidatedPaths {
        manifest: canonical_manifest,
        output: canonical_output,
        manifest_identity: FileIdentity {
            device: manifest_metadata.dev(),
            inode: manifest_metadata.ino(),
        },
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
        Ok(identity) if identity == paths.manifest_identity => Err(CliError::PathConflict),
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
    let paths = validate_distinct_paths(cli, repo_root)?;
    let manifest_bytes = fs::read(&paths.manifest).map_err(CliError::ReadManifest)?;
    let manifest = ReleaseManifest::from_slice(&manifest_bytes)?;
    let manifest_sha256 = format!("{:x}", Sha256::digest(&manifest_bytes));
    #[cfg(unix)]
    let output_target = prepare_output_target(&paths)?;

    let results = gate.run(repo_root, &manifest).await;
    let verdict = evaluate(&["security"], &results)?;
    let bundle = CertificationBundle::new(manifest_sha256, results, verdict);
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
        CertificationBundle::new(
            "0".repeat(64),
            vec![GateResult::new(
                "security",
                "check",
                true,
                GateStatus::Passed,
                1,
                vec![],
            )],
            CertificationVerdict::Passed,
        )
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
