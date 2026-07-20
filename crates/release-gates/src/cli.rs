use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Component, Path, PathBuf},
};

use serde::{de::Error as _, ser::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    evaluate, CertificationVerdict, GateResult, GateStatus, ManifestError, PolicyError,
    ProcessRunner, ReleaseManifest, SecurityGate,
};

pub const BUNDLE_SCHEMA_VERSION: u32 = 1;

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

    pub fn write_json(&self, path: impl AsRef<Path>, max_bytes: usize) -> Result<(), BundleError> {
        let path = path.as_ref();
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > max_bytes {
            return Err(BundleError::TooLarge {
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

pub async fn run_security<R>(
    cli: &Cli,
    repo_root: &Path,
    gate: &SecurityGate<R>,
) -> Result<CertificationBundle, CliError>
where
    R: ProcessRunner,
{
    let manifest_bytes = fs::read(&cli.manifest).map_err(CliError::ReadManifest)?;
    let manifest = ReleaseManifest::from_slice(&manifest_bytes)?;
    let manifest_sha256 = format!("{:x}", Sha256::digest(&manifest_bytes));

    if let Some(parent) = cli
        .output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(CliError::CreateOutputDirectory)?;
    }

    let results = gate.run(repo_root, &manifest).await;
    let verdict = evaluate(&["security"], &results)?;
    let bundle = CertificationBundle::new(manifest_sha256, results, verdict);
    bundle.write_json(&cli.output, manifest.security.max_output_bytes)?;
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
