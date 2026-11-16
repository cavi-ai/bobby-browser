//! Signed audit bundles: one workflow's journal lines, checkpoint, and stored
//! artifacts in a tar, with a manifest of SHA-256 digests signed by this
//! install's Ed25519 key.
//!
//! Export only reads the runtime's files, so it runs next to a live runtime.
//! Journal lines are copied byte for byte; nothing is re-serialized. Sealed
//! lines are opened first, so the bundle holds record JSON.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use types::{SessionId, WorkflowId};
use workflow_journal::JournalRecord;

const MANIFEST: &str = "manifest.json";
const SIGNATURE: &str = "signature.json";
const JOURNAL: &str = "journal.jsonl";
const CHECKPOINT: &str = "checkpoint.json";
const KIND: &str = "bobbyAuditBundle";
const KEY_FILE: &str = "audit-signing-key.pk8";
const MAX_ENTRIES: usize = 10_000;
const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    kind: String,
    workflow_id: String,
    session_ids: Vec<String>,
    created_at: DateTime<Utc>,
    bobby_version: String,
    files: Vec<FileEntry>,
    /// Artifacts the journal names that are no longer on disk (retention).
    missing_artifacts: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FileEntry {
    path: String,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Signature {
    algorithm: String,
    public_key: String,
    signature: String,
}

/// Where the runtime keeps what a bundle collects.
pub struct BundleSources {
    pub journal: PathBuf,
    pub checkpoints_dir: PathBuf,
    pub artifacts_dir: PathBuf,
    /// Key that opens sealed journal lines; they are bundled as plain records.
    pub journal_key: Option<PathBuf>,
}

impl BundleSources {
    pub fn from_config(config: &config::AppConfig) -> Self {
        Self {
            journal: config.storage.journal_path.clone(),
            checkpoints_dir: config.storage.checkpoints_dir.clone(),
            artifacts_dir: config.browser.artifacts_dir.clone(),
            journal_key: config.storage.journal_key_path.clone(),
        }
    }
}

#[derive(Debug)]
pub struct ExportSummary {
    pub path: PathBuf,
    pub files: usize,
    pub journal_lines: usize,
    pub missing_artifacts: Vec<String>,
    pub public_key: String,
}

#[derive(Debug)]
pub struct VerifySummary {
    pub workflow_id: String,
    pub files: usize,
    pub public_key: String,
    pub pinned: bool,
    pub missing_artifacts: Vec<String>,
}

/// This install's signing key: `<config dir>/audit-signing-key.pk8`.
pub fn default_key_path() -> Result<PathBuf> {
    config::bobby_config_dir()
        .map(|dir| dir.join(KEY_FILE))
        .ok_or_else(|| anyhow!("config directory unavailable; pass --key"))
}

/// Loads the PKCS#8 Ed25519 key at `path`, creating it (owner-only) on first use.
pub fn load_or_create_key(path: &Path) -> Result<Ed25519KeyPair> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let generated = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                .map_err(|_| anyhow!("could not generate an audit signing key"))?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let mut file = options
                .open(path)
                .with_context(|| format!("create {}", path.display()))?;
            file.write_all(generated.as_ref())?;
            file.sync_all()?;
            generated.as_ref().to_vec()
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    Ed25519KeyPair::from_pkcs8(&bytes)
        .map_err(|_| anyhow!("{} is not an Ed25519 PKCS#8 key", path.display()))
}

pub fn public_key_hex(key: &Ed25519KeyPair) -> String {
    hex::encode(key.public_key().as_ref())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// A path segment taken from the journal must not escape the bundle.
fn safe_segment(segment: &str) -> bool {
    !segment.is_empty() && segment != "." && segment != ".." && !segment.contains(['/', '\\', '\0'])
}

fn collect_artifact_ids(value: &serde_json::Value, ids: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                match (key.as_str(), value) {
                    ("artifactId", serde_json::Value::String(id)) => {
                        ids.insert(id.clone());
                    }
                    ("artifactIds", serde_json::Value::Array(values)) => {
                        ids.extend(
                            values
                                .iter()
                                .filter_map(|id| id.as_str().map(str::to_owned)),
                        );
                    }
                    _ => collect_artifact_ids(value, ids),
                }
            }
        }
        serde_json::Value::Array(values) => {
            values
                .iter()
                .for_each(|value| collect_artifact_ids(value, ids));
        }
        _ => {}
    }
}

/// Writes `workflow`'s bundle to `out`, which must not exist yet.
pub fn export(
    sources: &BundleSources,
    workflow: &WorkflowId,
    key: &Ed25519KeyPair,
    out: &Path,
) -> Result<ExportSummary> {
    let files = workflow_journal::journal_files(&sources.journal)
        .with_context(|| format!("list {}", sources.journal.display()))?;
    // Closed segments first, the active file last; a file may be absent.
    let mut contents: Vec<(Vec<u8>, bool)> = Vec::new();
    for (position, file) in files.iter().enumerate() {
        match std::fs::read(file) {
            Ok(bytes) => contents.push((bytes, position + 1 == files.len())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("read {}", file.display())),
        }
    }
    let journal_key = match &sources.journal_key {
        Some(path) => match workflow_journal::JournalKey::load(path) {
            Ok(key) => Some(key),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        },
        None => None,
    };
    // Every complete line of every file feeds one opener so the chain covers
    // the whole journal.
    let mut opener = workflow_journal::LineOpener::new(journal_key.as_ref());
    let mut parsed: Vec<(std::borrow::Cow<[u8]>, JournalRecord)> = Vec::new();
    // Only the active file may end in a write in progress.
    for (bytes, is_active) in &contents {
        let complete = if *is_active {
            bytes
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(&bytes[..0], |end| &bytes[..=end])
        } else {
            &bytes[..]
        };
        for line in complete.split_inclusive(|byte| *byte == b'\n') {
            let Some(line) = opener.open(line) else {
                continue;
            };
            if let Ok(record) = serde_json::from_slice::<JournalRecord>(&line) {
                parsed.push((line, record));
            }
        }
    }
    let mut commands = HashSet::new();
    // Keyed by the id's text so the manifest lists sessions in a stable order.
    let mut sessions: BTreeMap<String, SessionId> = BTreeMap::new();
    for (_, record) in &parsed {
        if let Some(envelope) = record
            .envelope
            .as_ref()
            .filter(|envelope| envelope.workflow_id == *workflow)
        {
            commands.insert(record.command_id.clone());
            sessions.insert(
                envelope.session_id.0.to_string(),
                envelope.session_id.clone(),
            );
        }
    }
    let mut journal_lines = Vec::new();
    let mut artifact_ids = BTreeSet::new();
    let mut line_count = 0;
    for (line, record) in &parsed {
        if commands.contains(&record.command_id) {
            journal_lines.extend_from_slice(line);
            line_count += 1;
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) {
                collect_artifact_ids(&value, &mut artifact_ids);
            }
        }
    }

    let checkpoint_path = checkpoint_store::checkpoint_path(&sources.checkpoints_dir, workflow);
    let checkpoint = match std::fs::read(&checkpoint_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", checkpoint_path.display()))
        }
    };
    if line_count == 0 && checkpoint.is_none() {
        bail!(
            "no journal records or checkpoint for workflow {} in {} and {}",
            workflow.0,
            sources.journal.display(),
            sources.checkpoints_dir.display()
        );
    }

    let mut files: Vec<(String, Vec<u8>)> = vec![(JOURNAL.to_owned(), journal_lines)];
    if let Some(checkpoint) = checkpoint {
        files.push((CHECKPOINT.to_owned(), checkpoint));
    }
    let mut missing_artifacts = Vec::new();
    for artifact_id in artifact_ids {
        if !safe_segment(&artifact_id) {
            bail!("journal names an unsafe artifact id {artifact_id:?}");
        }
        let directory = sessions
            .values()
            .map(|session| {
                artifact_store::artifact_dir(&sources.artifacts_dir, session, &artifact_id)
            })
            .find(|directory| directory.is_dir());
        let Some(directory) = directory else {
            missing_artifacts.push(artifact_id);
            continue;
        };
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&directory)
            .with_context(|| format!("read {}", directory.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<_>>()?;
        entries.sort();
        for path in entries {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if !path.is_file() || !safe_segment(name) || name.starts_with('.') {
                continue;
            }
            let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            files.push((format!("artifacts/{artifact_id}/{name}"), bytes));
        }
    }

    let manifest = Manifest {
        schema_version: 1,
        kind: KIND.to_owned(),
        workflow_id: workflow.0.to_string(),
        session_ids: sessions.keys().cloned().collect(),
        created_at: Utc::now(),
        bobby_version: env!("CARGO_PKG_VERSION").to_owned(),
        files: files
            .iter()
            .map(|(path, bytes)| FileEntry {
                path: path.clone(),
                bytes: bytes.len() as u64,
                sha256: sha256_hex(bytes),
            })
            .collect(),
        missing_artifacts: missing_artifacts.clone(),
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    let signature = Signature {
        algorithm: "ed25519".to_owned(),
        public_key: public_key_hex(key),
        signature: hex::encode(key.sign(&manifest_bytes).as_ref()),
    };
    let signature_bytes = serde_json::to_vec_pretty(&signature)?;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let file = options
        .open(out)
        .with_context(|| format!("create {} (it must not exist yet)", out.display()))?;
    let written = (|| -> Result<()> {
        let mut builder = tar::Builder::new(&file);
        let mut append = |path: &str, bytes: &[u8]| -> Result<()> {
            let mut header = tar::Header::new_ustar();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_uid(0);
            header.set_gid(0);
            header.set_entry_type(tar::EntryType::Regular);
            builder
                .append_data(&mut header, path, bytes)
                .with_context(|| format!("write {path} into the bundle"))
        };
        append(MANIFEST, &manifest_bytes)?;
        append(SIGNATURE, &signature_bytes)?;
        for (path, bytes) in &files {
            append(path, bytes)?;
        }
        builder.finish()?;
        Ok(())
    })()
    .and_then(|()| file.sync_all().map_err(Into::into));
    if let Err(error) = written {
        drop(file);
        let _ = std::fs::remove_file(out);
        return Err(error);
    }
    Ok(ExportSummary {
        path: out.to_path_buf(),
        files: files.len(),
        journal_lines: line_count,
        missing_artifacts,
        public_key: signature.public_key,
    })
}

/// A bundle whose digests and signature checked out, with its files.
pub struct VerifiedBundle {
    pub summary: VerifySummary,
    pub session_ids: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub bobby_version: String,
    /// Every file the manifest lists, by bundle path.
    pub files: BTreeMap<String, Vec<u8>>,
}

/// Checks every digest and the signature. With `pinned`, the bundle must be
/// signed by that public key (hex); without it, any valid signer passes and
/// the summary says so.
pub fn verify(bundle: &Path, pinned: Option<&str>) -> Result<VerifySummary> {
    open_verified(bundle, pinned).map(|verified| verified.summary)
}

/// [`verify`], keeping the verified files for a reader such as the replay.
pub fn open_verified(bundle: &Path, pinned: Option<&str>) -> Result<VerifiedBundle> {
    let file = std::fs::File::open(bundle).with_context(|| format!("open {}", bundle.display()))?;
    let mut archive = tar::Archive::new(file);
    let mut entries: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entries.len() >= MAX_ENTRIES {
            bail!("bundle has more than {MAX_ENTRIES} entries");
        }
        if entry.header().entry_type() != tar::EntryType::Regular {
            bail!("bundle holds a non-file entry");
        }
        let path = entry.path()?.to_string_lossy().into_owned();
        if !path.split('/').all(safe_segment) {
            bail!("bundle entry {path:?} is not a plain relative path");
        }
        let size = entry.header().size()?;
        if size > MAX_ENTRY_BYTES {
            bail!("{path}: {size} bytes is over the {MAX_ENTRY_BYTES} byte bound");
        }
        let mut bytes = Vec::with_capacity(size as usize);
        entry.read_to_end(&mut bytes)?;
        if entries.insert(path.clone(), bytes).is_some() {
            bail!("{path} appears twice");
        }
    }
    let manifest_bytes = entries
        .remove(MANIFEST)
        .ok_or_else(|| anyhow!("bundle has no {MANIFEST}"))?;
    let signature: Signature = serde_json::from_slice(
        &entries
            .remove(SIGNATURE)
            .ok_or_else(|| anyhow!("bundle has no {SIGNATURE}"))?,
    )
    .context("signature.json is malformed")?;
    if signature.algorithm != "ed25519" {
        bail!("unsupported signature algorithm {:?}", signature.algorithm);
    }
    let public_key = hex::decode(&signature.public_key).context("public key is not hex")?;
    let signature_bytes = hex::decode(&signature.signature).context("signature is not hex")?;
    UnparsedPublicKey::new(&ED25519, &public_key)
        .verify(&manifest_bytes, &signature_bytes)
        .map_err(|_| anyhow!("signature does not match manifest.json"))?;
    if let Some(pinned) = pinned {
        if !pinned.trim().eq_ignore_ascii_case(&signature.public_key) {
            bail!(
                "signed by {}, not the pinned key {}",
                signature.public_key,
                pinned.trim()
            );
        }
    }
    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).context("manifest.json is malformed")?;
    if manifest.schema_version != 1 || manifest.kind != KIND {
        bail!(
            "not a version 1 audit bundle (kind {:?}, schemaVersion {})",
            manifest.kind,
            manifest.schema_version
        );
    }
    let mut files = BTreeMap::new();
    for file in &manifest.files {
        let bytes = entries
            .remove(&file.path)
            .ok_or_else(|| anyhow!("{}: listed in the manifest but missing", file.path))?;
        if bytes.len() as u64 != file.bytes || sha256_hex(&bytes) != file.sha256 {
            bail!("{}: contents do not match the manifest digest", file.path);
        }
        files.insert(file.path.clone(), bytes);
    }
    if let Some(extra) = entries.keys().next() {
        bail!("{extra}: not listed in the manifest");
    }
    Ok(VerifiedBundle {
        summary: VerifySummary {
            workflow_id: manifest.workflow_id,
            files: manifest.files.len(),
            public_key: signature.public_key,
            pinned: pinned.is_some(),
            missing_artifacts: manifest.missing_artifacts,
        },
        session_ids: manifest.session_ids,
        created_at: manifest.created_at,
        bobby_version: manifest.bobby_version,
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use types::{
        AttemptId, CommandEnvelope, CommandId, CommandOutcome, CommandPhase, Evidence,
        NavigateCommand, PageId, PrimitiveCommand, RuntimeCommand, WaitUntil,
    };
    use workflow_journal::{CommandJournal, JsonlJournal};

    struct Fixture {
        _root: tempfile::TempDir,
        sources: BundleSources,
        workflow: WorkflowId,
        key: Ed25519KeyPair,
        out: PathBuf,
    }

    fn envelope(
        workflow: &WorkflowId,
        session: &SessionId,
        command: &CommandId,
    ) -> CommandEnvelope {
        CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: command.clone(),
            workflow_id: workflow.clone(),
            attempt_id: AttemptId::new(),
            session_id: session.clone(),
            page_id: Some(PageId::new()),
            deadline: Utc::now() + chrono::Duration::seconds(30),
            command: RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
                url: "https://shop.test/".into(),
                wait_until: WaitUntil::DomContentLoaded,
                timeout_ms: 10_000,
            })),
        }
    }

    fn record(
        command: &CommandId,
        phase: CommandPhase,
        envelope: Option<CommandEnvelope>,
        outcome: Option<CommandOutcome>,
    ) -> JournalRecord {
        JournalRecord {
            sequence: 0,
            recorded_at: Utc::now(),
            command_id: command.clone(),
            phase,
            envelope,
            outcome,
            prepared_result: None,
        }
    }

    /// Workflow W: one command with a screenshot, a checkpoint, and the
    /// screenshot's files. Workflow X: one unrelated command.
    async fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let sources = BundleSources {
            journal: root.path().join("commands.jsonl"),
            checkpoints_dir: root.path().join("checkpoints"),
            artifacts_dir: root.path().join("artifacts"),
            journal_key: None,
        };
        let workflow = WorkflowId::new();
        let session = SessionId::new();
        let ours = CommandId::new();
        let other = CommandId::new();
        let journal = JsonlJournal::open(&sources.journal).await.unwrap();
        journal
            .append(record(
                &ours,
                CommandPhase::Executing,
                Some(envelope(&workflow, &session, &ours)),
                None,
            ))
            .await
            .unwrap();
        journal
            .append(record(
                &other,
                CommandPhase::Executing,
                Some(envelope(&WorkflowId::new(), &session, &other)),
                None,
            ))
            .await
            .unwrap();
        journal
            .append(record(
                &ours,
                CommandPhase::Completed,
                None,
                Some(CommandOutcome::Completed {
                    command_id: ours.clone(),
                    evidence: vec![Evidence::Screenshot {
                        artifact_id: "shot-1".into(),
                        media_type: "image/png".into(),
                        width: 1,
                        height: 1,
                        bytes: 4,
                        sha256: sha256_hex(b"png!"),
                        origin: Default::default(),
                    }],
                }),
            ))
            .await
            .unwrap();
        std::fs::create_dir_all(&sources.checkpoints_dir).unwrap();
        std::fs::write(
            checkpoint_store::checkpoint_path(&sources.checkpoints_dir, &workflow),
            br#"{"checkpoint":"bytes as stored"}"#,
        )
        .unwrap();
        let artifact = artifact_store::artifact_dir(&sources.artifacts_dir, &session, "shot-1");
        std::fs::create_dir_all(&artifact).unwrap();
        std::fs::write(artifact.join("shot-1.png"), b"png!").unwrap();
        std::fs::write(artifact.join("shot-1.json"), b"{}").unwrap();
        let key = load_or_create_key(&root.path().join("key.pk8")).unwrap();
        let out = root.path().join("bundle.tar");
        Fixture {
            _root: root,
            sources,
            workflow,
            key,
            out,
        }
    }

    fn entries(bundle: &Path) -> Vec<(String, Vec<u8>)> {
        let mut archive = tar::Archive::new(std::fs::File::open(bundle).unwrap());
        archive
            .entries()
            .unwrap()
            .map(|entry| {
                let mut entry = entry.unwrap();
                let path = entry.path().unwrap().to_string_lossy().into_owned();
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).unwrap();
                (path, bytes)
            })
            .collect()
    }

    fn rewrite(bundle: &Path, entries: &[(String, Vec<u8>)]) {
        let file = std::fs::File::create(bundle).unwrap();
        let mut builder = tar::Builder::new(file);
        for (path, bytes) in entries {
            let mut header = tar::Header::new_ustar();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            builder.append_data(&mut header, path, &bytes[..]).unwrap();
        }
        builder.finish().unwrap();
    }

    #[tokio::test]
    async fn export_then_verify_round_trips_one_workflow_byte_for_byte() {
        let fixture = fixture().await;
        let summary = export(
            &fixture.sources,
            &fixture.workflow,
            &fixture.key,
            &fixture.out,
        )
        .unwrap();
        assert_eq!(summary.journal_lines, 2);
        assert_eq!(summary.files, 4);
        assert!(summary.missing_artifacts.is_empty());

        let bundle = entries(&fixture.out);
        let paths: Vec<&str> = bundle.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "manifest.json",
                "signature.json",
                "journal.jsonl",
                "checkpoint.json",
                "artifacts/shot-1/shot-1.json",
                "artifacts/shot-1/shot-1.png"
            ]
        );
        let journal = std::fs::read(&fixture.sources.journal).unwrap();
        let expected: Vec<u8> = journal
            .split_inclusive(|byte| *byte == b'\n')
            .filter(|line| {
                std::str::from_utf8(line)
                    .unwrap()
                    .contains(&fixture.workflow.0.to_string())
                    || std::str::from_utf8(line).unwrap().contains("shot-1")
            })
            .flatten()
            .copied()
            .collect();
        assert_eq!(
            bundle[2].1, expected,
            "journal lines must be copied verbatim"
        );
        assert_eq!(bundle[3].1, br#"{"checkpoint":"bytes as stored"}"#);

        let verified = verify(&fixture.out, None).unwrap();
        assert_eq!(verified.workflow_id, fixture.workflow.0.to_string());
        assert_eq!(verified.files, 4);
        assert!(!verified.pinned);
        let signer = public_key_hex(&fixture.key);
        assert!(verify(&fixture.out, Some(&signer)).unwrap().pinned);
        let stranger =
            public_key_hex(&load_or_create_key(&fixture._root.path().join("other.pk8")).unwrap());
        let error = verify(&fixture.out, Some(&stranger))
            .unwrap_err()
            .to_string();
        assert!(error.contains("not the pinned key"), "{error}");
    }

    #[tokio::test]
    async fn export_from_a_sealed_journal_bundles_plain_records() {
        let root = tempfile::tempdir().unwrap();
        let key_path = root.path().join("journal.key");
        let sources = BundleSources {
            journal: root.path().join("commands.jsonl"),
            checkpoints_dir: root.path().join("checkpoints"),
            artifacts_dir: root.path().join("artifacts"),
            journal_key: Some(key_path.clone()),
        };
        let workflow = WorkflowId::new();
        let command = CommandId::new();
        let journal = JsonlJournal::open_sealed(
            &sources.journal,
            workflow_journal::JournalKey::load_or_create(&key_path).unwrap(),
        )
        .await
        .unwrap();
        let mut sealed = envelope(&workflow, &SessionId::new(), &command);
        sealed.command = RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
            url: "https://shop.test/typed-marker-1f3a".into(),
            wait_until: WaitUntil::DomContentLoaded,
            timeout_ms: 10_000,
        }));
        journal
            .append(record(
                &command,
                CommandPhase::Executing,
                Some(sealed),
                None,
            ))
            .await
            .unwrap();
        journal
            .append(record(&command, CommandPhase::Accepted, None, None))
            .await
            .unwrap();
        let marker = b"typed-marker-1f3a";
        assert!(!std::fs::read(&sources.journal)
            .unwrap()
            .windows(marker.len())
            .any(|window| window == marker));

        let key = load_or_create_key(&root.path().join("key.pk8")).unwrap();
        let out = root.path().join("bundle.tar");
        let summary = export(&sources, &workflow, &key, &out).unwrap();
        assert_eq!(summary.journal_lines, 2);
        verify(&out, None).unwrap();
        let bundle = entries(&out);
        let text = String::from_utf8(bundle[2].1.clone()).unwrap();
        assert_eq!(bundle[2].0, "journal.jsonl");
        assert!(text.contains("typed-marker-1f3a"), "{text}");
        assert!(text.ends_with('\n') && !text.ends_with("\n\n"), "{text:?}");
        assert!(text.lines().all(|line| !line.is_empty()), "{text:?}");
        assert_eq!(text.lines().count(), 2);
        assert!(text
            .lines()
            .all(|line| serde_json::from_str::<JournalRecord>(line).is_ok()));
    }

    #[tokio::test]
    async fn export_from_a_segmented_sealed_journal_bundles_every_record_of_the_workflow() {
        let root = tempfile::tempdir().unwrap();
        let key_path = root.path().join("journal.key");
        let sources = BundleSources {
            journal: root.path().join("commands.jsonl"),
            checkpoints_dir: root.path().join("checkpoints"),
            artifacts_dir: root.path().join("artifacts"),
            journal_key: Some(key_path.clone()),
        };
        let workflow = WorkflowId::new();
        let session = SessionId::new();
        let ours = CommandId::new();
        let other = CommandId::new();
        let journal = JsonlJournal::open_with(
            &sources.journal,
            workflow_journal::JournalOptions {
                segment_bytes: 1,
                key: Some(workflow_journal::JournalKey::load_or_create(&key_path).unwrap()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let first = envelope(&workflow, &session, &ours);
        let unrelated = envelope(&WorkflowId::new(), &session, &other);
        for (id, phase, envelope) in [
            (&ours, CommandPhase::Executing, Some(first)),
            (&other, CommandPhase::Executing, Some(unrelated)),
            (&ours, CommandPhase::Accepted, None),
        ] {
            journal
                .append(record(id, phase, envelope, None))
                .await
                .unwrap();
        }
        let closed = workflow_journal::journal_files(&sources.journal).unwrap();
        assert!(closed.len() > 1, "{closed:?}");
        assert!(closed[0].exists());

        let key = load_or_create_key(&root.path().join("key.pk8")).unwrap();
        let out = root.path().join("bundle.tar");
        let summary = export(&sources, &workflow, &key, &out).unwrap();
        assert_eq!(summary.journal_lines, 2);
        verify(&out, None).unwrap();
        let bundle = entries(&out);
        let text = String::from_utf8(bundle[2].1.clone()).unwrap();
        assert_eq!(bundle[2].0, "journal.jsonl");
        let records: Vec<JournalRecord> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|record| record.command_id == ours));
        assert!(records[0].sequence < records[1].sequence);
    }

    #[tokio::test]
    async fn verify_names_a_tampered_an_extra_and_a_missing_file() {
        let fixture = fixture().await;
        export(
            &fixture.sources,
            &fixture.workflow,
            &fixture.key,
            &fixture.out,
        )
        .unwrap();
        let original = entries(&fixture.out);

        let mut tampered = original.clone();
        tampered[5].1 = b"PNG!".to_vec();
        rewrite(&fixture.out, &tampered);
        let error = verify(&fixture.out, None).unwrap_err().to_string();
        assert!(error.contains("artifacts/shot-1/shot-1.png"), "{error}");

        let mut extra = original.clone();
        extra.push(("notes.txt".into(), b"added later".to_vec()));
        rewrite(&fixture.out, &extra);
        let error = verify(&fixture.out, None).unwrap_err().to_string();
        assert!(error.contains("notes.txt: not listed"), "{error}");

        let mut missing = original.clone();
        missing.remove(3);
        rewrite(&fixture.out, &missing);
        let error = verify(&fixture.out, None).unwrap_err().to_string();
        assert!(
            error.contains("checkpoint.json: listed in the manifest but missing"),
            "{error}"
        );

        let mut resigned_manifest = original.clone();
        resigned_manifest[0].1 = String::from_utf8(resigned_manifest[0].1.clone())
            .unwrap()
            .replace("\"missingArtifacts\": []", "\"missingArtifacts\": [\"x\"]")
            .into_bytes();
        rewrite(&fixture.out, &resigned_manifest);
        let error = verify(&fixture.out, None).unwrap_err().to_string();
        assert!(error.contains("signature does not match"), "{error}");

        rewrite(&fixture.out, &original);
        verify(&fixture.out, None).unwrap();
    }

    #[tokio::test]
    async fn export_refuses_an_existing_output_and_an_unknown_workflow() {
        let fixture = fixture().await;
        std::fs::write(&fixture.out, b"keep me").unwrap();
        let error = export(
            &fixture.sources,
            &fixture.workflow,
            &fixture.key,
            &fixture.out,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("must not exist"), "{error}");
        assert_eq!(std::fs::read(&fixture.out).unwrap(), b"keep me");

        let unknown = fixture.out.with_file_name("unknown.tar");
        let error = export(&fixture.sources, &WorkflowId::new(), &fixture.key, &unknown)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("no journal records or checkpoint"),
            "{error}"
        );
        assert!(!unknown.exists());
    }

    #[test]
    fn the_signing_key_is_owner_only_and_reused() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("nested").join("key.pk8");
        let first = public_key_hex(&load_or_create_key(&path).unwrap());
        let second = public_key_hex(&load_or_create_key(&path).unwrap());
        assert_eq!(first, second);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
