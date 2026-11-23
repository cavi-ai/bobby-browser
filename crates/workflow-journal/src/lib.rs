use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError};

use async_trait::async_trait;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio::sync::{oneshot, Mutex};
use tracing::warn;
use types::{AttemptId, CommandEnvelope, CommandId, CommandOutcome, CommandPhase, Evidence};

/// Default physical JSONL record bound, including the trailing newline.
pub const DEFAULT_MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;

/// Active file size at which the journal rolls into a closed segment.
pub const DEFAULT_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;

/// Limits and sealing key for [`JsonlJournal::open_with`].
#[derive(Debug)]
pub struct JournalOptions {
    pub max_record_bytes: usize,
    pub segment_bytes: u64,
    pub key: Option<JournalKey>,
}

impl Default for JournalOptions {
    fn default() -> Self {
        Self {
            max_record_bytes: DEFAULT_MAX_RECORD_BYTES,
            segment_bytes: DEFAULT_SEGMENT_BYTES,
            key: None,
        }
    }
}

mod health;
mod seal;
mod segments;
pub use health::{
    scan_jsonl, scan_jsonl_prefetched, LedgerDecoder, LedgerHealth, LedgerIssue, LedgerIssueKind,
    PreparedRecord, RecordObservation, ScanControl, ScanOptions, SequenceRule, SequenceVerdict,
};
pub use seal::{JournalKey, LineOpener};
pub use segments::journal_files;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedResult {
    pub command_id: CommandId,
    pub attempt_id: AttemptId,
    pub state_version: u64,
    pub state_delta: serde_json::Value,
    pub evidence: Vec<Evidence>,
    pub artifact_id: Option<String>,
    pub artifact_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_staging_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download: Option<PreparedDownload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedDownload {
    /// Opaque identifier for implementation-owned state below the downloads root.
    pub staging_id: String,
    /// Digest of the protected sidecar needed to recover caller-selected metadata.
    pub metadata_sha256: String,
}

#[async_trait]
pub trait CommandJournal: Send + Sync {
    async fn append(&self, record: JournalRecord) -> Result<(), JournalError>;
    async fn history(&self, id: CommandId) -> Result<JournalScan, JournalError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalRecord {
    pub sequence: u64,
    pub recorded_at: types::WireTimestamp,
    pub command_id: CommandId,
    pub phase: CommandPhase,
    pub envelope: Option<CommandEnvelope>,
    pub outcome: Option<CommandOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared_result: Option<PreparedResult>,
}

#[derive(Debug, Clone, Default)]
pub struct JournalScan {
    pub records: Vec<JournalRecord>,
    pub torn_tail: bool,
    /// Unreadable lines, or an archived history whose integrity cannot be
    /// established. Any nonzero value makes records diagnostic only.
    pub incompatible_records: usize,
}

/// Trusted records across closed segments and the active file, one chain.
/// Only the active file may end in a write in progress; that tail is not a record.
pub fn read_trusted_records(
    path: &Path,
    key: Option<&JournalKey>,
) -> std::io::Result<Vec<(Vec<u8>, JournalRecord)>> {
    let files = segments::journal_files(path)?;
    let mut opener = LineOpener::new(key);
    let mut records = Vec::new();
    for (position, file) in files.iter().enumerate() {
        let bytes = match std::fs::read(file) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let is_active = position + 1 == files.len();
        let complete: &[u8] = if is_active {
            match bytes.iter().rposition(|byte| *byte == b'\n') {
                Some(end) => &bytes[..=end],
                None => &[],
            }
        } else {
            &bytes
        };
        for line in complete.split_inclusive(|byte| *byte == b'\n') {
            let Some(opened) = opener.open(line) else {
                continue;
            };
            if let Ok(record) = serde_json::from_slice::<JournalRecord>(&opened) {
                records.push((opened.into_owned(), record));
            }
        }
    }
    Ok(records)
}

/// Read-only health of a journal file. Never truncates or creates the path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JournalHealth {
    pub exists: bool,
    pub bytes: u64,
    pub records: usize,
    pub torn_tail: bool,
    pub incompatible_records: usize,
    /// Closed segment files beside the active file.
    pub segments: usize,
}

/// The sequence of a journal line this build cannot decode, when present.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecordProbe {
    #[serde(default)]
    sequence: Option<u64>,
}

/// Diagnostic identity only; never restores a record's execution authority.
struct ArchiveRecordProbe {
    command_id: Option<CommandId>,
}

impl<'de> Deserialize<'de> for ArchiveRecordProbe {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ProbeVisitor;

        impl<'de> serde::de::Visitor<'de> for ProbeVisitor {
            type Value = ArchiveRecordProbe;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a journal record object")
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut command_id = None;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "commandId" {
                        // Match Value's last-key-wins behavior, including a final
                        // invalid ID. Only this field needs to be materialized.
                        let value = map.next_value::<serde_json::Value>()?;
                        command_id = serde_json::from_value(value).ok();
                    } else {
                        map.next_value::<serde::de::IgnoredAny>()?;
                    }
                }
                Ok(ArchiveRecordProbe { command_id })
            }
        }

        deserializer.deserialize_map(ProbeVisitor)
    }
}

struct Scan {
    scan: JournalScan,
    /// Decodable complete records, including records with invalid sequence order.
    records: usize,
    /// Highest sequence in the file, including skipped lines, so appends stay monotonic.
    max_sequence: Option<u64>,
    /// Digest of the last complete line, for chaining the next append.
    last_digest: [u8; 32],
    /// Whether a sealed line exists at or before the end of the file.
    sealed_seen: bool,
    first_sequence: Option<u64>,
}

/// Where a file's chain and sequence order continue from.
#[derive(Clone, Copy)]
struct ScanStart {
    previous: [u8; 32],
    sealed_seen: bool,
    /// Sequences below this are not valid in the scanned file.
    next_sequence: u64,
}

impl ScanStart {
    fn genesis() -> Self {
        Self {
            previous: seal::GENESIS,
            sealed_seen: false,
            next_sequence: 0,
        }
    }
}

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("journal I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("journal serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("command belongs to archived, uncertain journal history")]
    UncertainCommand,
}

#[derive(Clone)]
pub struct JsonlJournal {
    path: Arc<PathBuf>,
    writer: Arc<Mutex<WriterState>>,
    max_record_bytes: usize,
    segment_bytes: u64,
    key: Option<Arc<JournalKey>>,
    queue: Arc<std::sync::Mutex<Vec<Pending>>>,
    appended: Arc<AtomicU64>,
    syncs: Arc<AtomicU64>,
}

type Reply = oneshot::Sender<Result<(), JournalError>>;

struct Pending {
    record: JournalRecord,
    reply: Reply,
}

struct Staged {
    command_id: CommandId,
    sequence: u64,
    indexed: IndexedRecord,
    reply: Reply,
}

/// Gives each waiting caller its own copy of a failure shared by the batch.
fn reply_all(replies: impl IntoIterator<Item = Reply>, error: &JournalError) {
    for reply in replies {
        let copy = match error {
            JournalError::Io(error) => {
                JournalError::Io(std::io::Error::new(error.kind(), error.to_string()))
            }
            JournalError::Serialization(error) => {
                JournalError::Io(std::io::Error::other(error.to_string()))
            }
            JournalError::UncertainCommand => JournalError::UncertainCommand,
        };
        let _ = reply.send(Err(copy));
    }
}

/// Counts since this journal was opened.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JournalStats {
    pub appends: u64,
    pub syncs: u64,
}

struct WriterState {
    file: File,
    identity: same_file::Handle,
    next_sequence: u64,
    file_len: u64,
    /// Digest of the last complete line in the active file, or of the last
    /// closed segment's tail while the active file is empty.
    last_digest: [u8; 32],
    /// Digest the active file's first line chains from.
    active_previous: [u8; 32],
    /// Whether a sealed line exists at or before the end of the active file.
    sealed_seen: bool,
    active_first_sequence: Option<u64>,
    active_records: usize,
    /// Highest closed segment number.
    segment_count: u32,
    modified: Option<std::time::SystemTime>,
    offsets: HashMap<CommandId, Vec<IndexedRecord>>,
    archives: Vec<Arc<PathBuf>>,
    archived_offsets: HashMap<CommandId, Vec<ArchivedRecordLocation>>,
    archived_torn_tail: bool,
    archived_incompatible_records: usize,
}

/// Derived bounds keep a changed delimiter from expanding a diagnostic read.
struct ArchivedRecordLocation {
    path: Arc<PathBuf>,
    offset: u64,
    len: u64,
    /// Digest of the line before this one in its file.
    previous: [u8; 32],
    /// Whether a plain line was acceptable at this position.
    plain_allowed: bool,
}

/// Rebuildable checks against the bytes validated at open or durably appended.
/// Length bounds indexed reads even if a delimiter is subsequently damaged.
struct IndexedRecord {
    offset: u64,
    len: usize,
    digest: [u8; 32],
    /// Digest of the line before this one in its file.
    previous: [u8; 32],
    /// The closed segment holding the record; `None` is the active file.
    segment: Option<Arc<PathBuf>>,
}

impl IndexedRecord {
    fn new(offset: u64, bytes: &[u8], previous: [u8; 32]) -> Self {
        Self {
            offset,
            len: bytes.len(),
            digest: Sha256::digest(bytes).into(),
            previous,
            segment: None,
        }
    }
}

async fn read_indexed_records(
    file: &File,
    entries: &[IndexedRecord],
    id: &CommandId,
    key: Option<&JournalKey>,
) -> Result<Option<Vec<JournalRecord>>, JournalError> {
    let mut reader = BufReader::new(file.try_clone().await?);
    let mut records = Vec::with_capacity(entries.len());
    // The cloned handle may share an existing file position; always seek first.
    let mut position = None;
    let mut line = Vec::new();
    for entry in entries {
        if position != Some(entry.offset) {
            reader.seek(std::io::SeekFrom::Start(entry.offset)).await?;
        }
        line.resize(entry.len, 0);
        if let Err(error) = reader.read_exact(&mut line).await {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                return Ok(None);
            }
            return Err(error.into());
        }
        position = Some(entry.offset + entry.len as u64);
        let digest: [u8; 32] = Sha256::digest(&line).into();
        if !line.ends_with(b"\n") || digest != entry.digest {
            return Ok(None);
        }
        // The digest above proves these are the bytes accepted when indexed.
        let Some(plain) = seal::open_at(&line, key, &entry.previous, true) else {
            return Ok(None);
        };
        let Ok(record) = serde_json::from_slice::<JournalRecord>(&plain) else {
            return Ok(None);
        };
        if &record.command_id != id {
            return Ok(None);
        }
        records.push(record);
    }
    Ok(Some(records))
}

/// Preserve damaged bytes intact before opening a fresh journal. No repair is
/// inferred from the readable prefix: its missing outcomes remain uncertain.
pub async fn archive_damaged_journal(path: &Path) -> Result<PathBuf, std::io::Error> {
    archive_entry(path, path).await
}

/// Renames `target`, the journal or one of its segments, to
/// `<journal file name>.archive-<id>` beside the journal.
async fn archive_entry(journal: &Path, target: &Path) -> Result<PathBuf, std::io::Error> {
    let name = journal
        .file_name()
        .ok_or_else(|| std::io::Error::other("journal has no filename"))?;
    let archive = journal.with_file_name(format!(
        "{}.archive-{}",
        name.to_string_lossy(),
        CommandId::new().0
    ));
    tokio::fs::rename(target, &archive).await?;
    sync_parent(journal).await?;
    // Also visible for library consumers without a tracing subscriber.
    eprintln!("WARNING: damaged journal {} archived intact to {}; starting a fresh journal. Archived operations require reconciliation.", target.display(), archive.display());
    warn!(path = %target.display(), archive = %archive.display(), "damaged journal archived; starting a fresh journal; old operations require reconciliation");
    Ok(archive)
}

async fn sync_parent(path: &Path) -> Result<(), std::io::Error> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        File::open(parent).await?.sync_all().await?;
    }
    Ok(())
}

impl JsonlJournal {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, JournalError> {
        Self::open_with(path, JournalOptions::default()).await
    }

    /// Configure the record bound for an existing or newly created journal.
    /// Oversized stored records are archived; oversized appends are rejected.
    pub async fn open_with_max_record_bytes(
        path: impl AsRef<Path>,
        max_record_bytes: usize,
    ) -> Result<Self, JournalError> {
        Self::open_with(
            path,
            JournalOptions {
                max_record_bytes,
                ..JournalOptions::default()
            },
        )
        .await
    }

    /// Open a journal whose new records are sealed with `key`. Existing
    /// plaintext records stay readable.
    pub async fn open_sealed(
        path: impl AsRef<Path>,
        key: JournalKey,
    ) -> Result<Self, JournalError> {
        Self::open_with(
            path,
            JournalOptions {
                key: Some(key),
                ..JournalOptions::default()
            },
        )
        .await
    }

    /// Open with explicit limits and an optional sealing key.
    pub async fn open_with(
        path: impl AsRef<Path>,
        options: JournalOptions,
    ) -> Result<Self, JournalError> {
        let JournalOptions {
            max_record_bytes,
            segment_bytes,
            key,
        } = options;
        let key = key.map(Arc::new);
        validate_record_limit(max_record_bytes)?;
        if segment_bytes == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "journal segment byte limit must be positive",
            )
            .into());
        }
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        let writer = open_writer(&path, max_record_bytes, key.as_deref()).await?;
        Ok(Self {
            path: Arc::new(path),
            writer: Arc::new(Mutex::new(writer)),
            max_record_bytes,
            segment_bytes,
            key,
            queue: Arc::new(std::sync::Mutex::new(Vec::new())),
            appended: Arc::new(AtomicU64::new(0)),
            syncs: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Counts of records appended and syncs issued by this journal and its clones.
    pub fn stats(&self) -> JournalStats {
        JournalStats {
            appends: self.appended.load(Ordering::Relaxed),
            syncs: self.syncs.load(Ordering::Relaxed),
        }
    }

    /// Archives are diagnostic data, never command authority.
    pub async fn archives(&self) -> Vec<PathBuf> {
        self.writer
            .lock()
            .await
            .archives
            .iter()
            .map(|path| path.as_ref().clone())
            .collect()
    }

    pub async fn inspect(path: impl AsRef<Path>) -> Result<JournalHealth, JournalError> {
        Self::inspect_with_max_record_bytes(path, DEFAULT_MAX_RECORD_BYTES).await
    }

    /// Inspect using the same bound supplied when opening the journal.
    pub async fn inspect_with_max_record_bytes(
        path: impl AsRef<Path>,
        max_record_bytes: usize,
    ) -> Result<JournalHealth, JournalError> {
        Self::inspect_inner(path.as_ref(), max_record_bytes, None).await
    }

    /// Inspect a journal that may hold records sealed with `key`.
    pub async fn inspect_sealed(
        path: impl AsRef<Path>,
        key: &JournalKey,
    ) -> Result<JournalHealth, JournalError> {
        Self::inspect_inner(path.as_ref(), DEFAULT_MAX_RECORD_BYTES, Some(key)).await
    }

    async fn inspect_inner(
        path: &Path,
        max_record_bytes: usize,
        key: Option<&JournalKey>,
    ) -> Result<JournalHealth, JournalError> {
        validate_record_limit(max_record_bytes)?;
        let closed = segments::inspect(path, key).await?;
        let file = match open_regular_read(path).await {
            Ok(Some(file)) => file,
            Ok(None) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "journal must be a regular file",
                )
                .into());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if closed.segments == 0 {
                    return Ok(JournalHealth::default());
                }
                return Ok(JournalHealth {
                    exists: true,
                    bytes: closed.bytes,
                    records: closed.records,
                    torn_tail: false,
                    incompatible_records: closed.incompatible_records,
                    segments: closed.segments,
                });
            }
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata().await?;
        let Scan { scan, records, .. } =
            scan_file(file, None, max_record_bytes, key, closed.start).await?;
        Ok(JournalHealth {
            exists: true,
            bytes: closed.bytes + metadata.len(),
            records: closed.records + records,
            torn_tail: scan.torn_tail,
            incompatible_records: closed.incompatible_records + scan.incompatible_records,
            segments: closed.segments,
        })
    }

    /// Writes `batch` with one write and one sync and replies to every caller
    /// after the outcome is known. Index state changes only after the sync.
    async fn commit_batch(&self, writer: &mut WriterState, batch: Vec<Pending>) {
        if let Err(error) = self.refresh(writer).await {
            return reply_all(batch.into_iter().map(|pending| pending.reply), &error);
        }
        if writer.file_len > 0 && writer.file_len >= self.segment_bytes {
            if let Err(error) = segments::roll(&self.path, writer, self.key.as_deref()).await {
                return reply_all(batch.into_iter().map(|pending| pending.reply), &error);
            }
        }
        let mut next_sequence = writer.next_sequence;
        let mut last_digest = writer.last_digest;
        let mut offset = writer.file_len;
        let mut buffer = Vec::new();
        let mut staged: Vec<Staged> = Vec::with_capacity(batch.len());
        for Pending { mut record, reply } in batch {
            if writer.archived_offsets.contains_key(&record.command_id) {
                let _ = reply.send(Err(JournalError::UncertainCommand));
                continue;
            }
            let Some(next) = next_sequence.checked_add(1) else {
                let _ = reply.send(Err(JournalError::UncertainCommand));
                continue;
            };
            record.sequence = next_sequence;
            let line = match self.encode_line(&record, &last_digest) {
                Ok(line) => line,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    continue;
                }
            };
            let indexed = IndexedRecord::new(offset, &line, last_digest);
            last_digest = indexed.digest;
            offset += line.len() as u64;
            next_sequence = next;
            buffer.extend_from_slice(&line);
            staged.push(Staged {
                command_id: record.command_id,
                sequence: record.sequence,
                indexed,
                reply,
            });
        }
        if staged.is_empty() {
            return;
        }
        if let Err(error) = self.write_synced(writer, &buffer).await {
            return reply_all(staged.into_iter().map(|staged| staged.reply), &error);
        }
        self.syncs.fetch_add(1, Ordering::Relaxed);
        self.appended
            .fetch_add(staged.len() as u64, Ordering::Relaxed);
        writer
            .active_first_sequence
            .get_or_insert(staged[0].sequence);
        writer.active_records += staged.len();
        writer.sealed_seen |= self.key.is_some();
        writer.last_digest = last_digest;
        writer.file_len = offset;
        writer.next_sequence = next_sequence;
        writer.modified = writer
            .file
            .metadata()
            .await
            .ok()
            .and_then(|metadata| metadata.modified().ok());
        for Staged {
            command_id,
            indexed,
            reply,
            ..
        } in staged
        {
            writer.offsets.entry(command_id).or_default().push(indexed);
            let _ = reply.send(Ok(()));
        }
    }

    fn encode_line(
        &self,
        record: &JournalRecord,
        previous: &[u8; 32],
    ) -> Result<Vec<u8>, JournalError> {
        let mut buffer = durable_file::BoundedBuffer::new(self.max_record_bytes);
        serde_json::to_writer(&mut buffer, record)?;
        std::io::Write::write_all(&mut buffer, b"\n")?;
        let mut bytes = buffer.into_inner();
        if let Some(key) = self.key.as_deref() {
            bytes = key.seal_line(&bytes, previous)?;
            if bytes.len() > self.max_record_bytes {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "serialized data exceeds byte limit",
                )
                .into());
            }
        }
        Ok(bytes)
    }

    async fn write_synced(
        &self,
        writer: &mut WriterState,
        bytes: &[u8],
    ) -> Result<(), JournalError> {
        durable_file::require_sole_hard_link(&writer.file.metadata().await?, "journal entry")?;
        writer.file.write_all(bytes).await?;
        writer.file.flush().await?;
        writer.file.sync_data().await?;
        Ok(())
    }

    async fn refresh(&self, writer: &mut WriterState) -> Result<(), JournalError> {
        match open_regular_read(&self.path).await {
            Ok(Some(file)) => {
                let metadata = file.metadata().await?;
                let identity = file_identity(file).await?;
                // Size and mtime may survive atomic replacement. The index and
                // append handle must still refer to the file at the current path.
                if identity == writer.identity
                    && durable_file::require_sole_hard_link(&metadata, "journal entry").is_ok()
                    && metadata.len() == writer.file_len
                    && metadata.modified().ok() == writer.modified
                {
                    return Ok(());
                }
            }
            Ok(None) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        *writer = open_writer(&self.path, self.max_record_bytes, self.key.as_deref()).await?;
        Ok(())
    }
}

fn validate_record_limit(max_bytes: usize) -> std::io::Result<()> {
    if max_bytes == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "journal record byte limit must be positive",
        ));
    }
    Ok(())
}

async fn file_identity(file: File) -> Result<same_file::Handle, std::io::Error> {
    let file = file.into_std().await;
    // Identity uses device/inode on Unix and volume/file index on Windows.
    // Keep its filesystem metadata call off the async runtime threads.
    tokio::task::spawn_blocking(move || same_file::Handle::from_file(file))
        .await
        .map_err(std::io::Error::other)?
}

/// Never follow a link or read a stream. Validate the opened handle too,
/// since the path can be replaced after the first metadata check.
async fn open_regular_read(path: &Path) -> Result<Option<File>, std::io::Error> {
    if !tokio::fs::symlink_metadata(path).await?.is_file() {
        return Ok(None);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    durable_file::without_following_links_async(&mut options);
    let file = options.open(path).await?;
    if durable_file::require_regular_file(&file.metadata().await?, "journal entry").is_err() {
        return Ok(None);
    }
    Ok(Some(file))
}

/// Opens or creates the owner-only active file, never following a link.
async fn open_active(path: &Path) -> Result<File, std::io::Error> {
    let mut options = OpenOptions::new();
    options.create(true).append(true).read(true);
    durable_file::without_following_links_async(&mut options);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(path).await?;
    durable_file::require_regular_file(&file.metadata().await?, "journal entry")?;
    durable_file::require_sole_hard_link(&file.metadata().await?, "journal entry")?;
    Ok(file)
}

async fn open_archive(path: &Path) -> Result<Option<File>, std::io::Error> {
    let file = open_regular_read(path).await?;
    if file.is_none() {
        warn!(path = %path.display(), "non-regular journal archive preserved without reading; history remains uncertain");
    }
    Ok(file)
}

async fn open_writer(
    path: &Path,
    max_record_bytes: usize,
    key: Option<&JournalKey>,
) -> Result<WriterState, JournalError> {
    let closed = segments::load(path, max_record_bytes, key).await?;
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata)
            if !metadata.is_file()
                || durable_file::require_sole_hard_link(&metadata, "journal entry").is_err() =>
        {
            // Preserve the entry itself: renaming a symlink does not change
            // its target, and a FIFO must never be opened for scanning.
            segments::archive_active(path, Some(closed.tail), key).await?;
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut file = open_active(path).await?;
    let mut active_offsets = HashMap::<CommandId, Vec<IndexedRecord>>::new();
    // Validate and index the writer's actual file in one streaming pass,
    // continuing the chain and sequence order of the closed segments.
    let scan = scan_file(
        file.try_clone().await?,
        Some(&mut active_offsets),
        max_record_bytes,
        key,
        closed.start(),
    )
    .await?;
    let mut last_digest = scan.last_digest;
    let mut sealed_seen = scan.sealed_seen;
    let mut active_first_sequence = scan.first_sequence;
    let mut active_records = scan.records;
    let next_sequence = if scan.scan.torn_tail
        || scan.scan.incompatible_records > 0
        || scan.max_sequence == Some(u64::MAX)
    {
        segments::archive_active(path, Some(closed.tail), key).await?;
        active_offsets.clear();
        file = open_active(path).await?;
        last_digest = closed.tail;
        sealed_seen = closed.sealed_seen;
        active_first_sequence = None;
        active_records = 0;
        closed.next_sequence
    } else {
        scan.max_sequence
            .map_or(closed.next_sequence, |sequence| sequence + 1)
            .max(closed.next_sequence)
    };
    let mut offsets = closed.entries;
    for (id, records) in active_offsets {
        offsets.entry(id).or_default().extend(records);
    }
    file.sync_all().await?;
    sync_parent(path).await?;
    let metadata = file.metadata().await?;
    let identity = file_identity(file.try_clone().await?).await?;
    let mut archives = Vec::new();
    let mut archived_offsets = HashMap::<CommandId, Vec<ArchivedRecordLocation>>::new();
    let mut archived_torn_tail = false;
    let mut archived_incompatible_records = 0;
    let prefix = format!("{}.archive-", path.file_name().unwrap().to_string_lossy());
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut entries = tokio::fs::read_dir(parent).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Sidecars of archived segments are not archive bodies.
        if !name.starts_with(&prefix)
            || name.ends_with(".index")
            || name.ends_with(".index.damaged")
        {
            continue;
        }
        let archive = Arc::new(entry.path());
        let Some(file) = open_archive(archive.as_ref()).await? else {
            archived_incompatible_records += 1;
            archives.push(archive);
            continue;
        };
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        let mut offset = 0;
        // An archived file that continued a chain records where it started.
        let mut opener = match segments::archive_start(archive.as_ref(), key).await? {
            Some(start) => LineOpener::resume(key, start, false),
            None => LineOpener::new(key),
        };
        loop {
            let read = (&mut reader)
                .take((max_record_bytes as u64).saturating_add(1))
                .read_until(b'\n', &mut line)
                .await?;
            if read == 0 {
                break;
            }
            if line.len() > max_record_bytes {
                archived_incompatible_records += 1;
                offset += line.len() as u64;
                // At the limit + 1 boundary the newline may already be consumed.
                // Only drain the remainder when this is still the same line.
                if !line.ends_with(b"\n") {
                    offset += discard_jsonl_line(&mut reader).await?;
                }
                line.clear();
                continue;
            }
            if !line.ends_with(b"\n") {
                archived_torn_tail = true;
            }
            let previous = opener.previous();
            let plain_allowed = !opener.sealed_seen();
            let plain = if line.ends_with(b"\n") {
                opener.open(&line)
            } else {
                seal::open_at(&line, key, &previous, plain_allowed)
            };
            if !line.iter().all(u8::is_ascii_whitespace) {
                let id = match plain
                    .as_deref()
                    .map(serde_json::from_slice::<JournalRecord>)
                {
                    Some(Ok(record)) => Some(record.command_id),
                    _ => {
                        archived_incompatible_records += 1;
                        // Incompatible records still reserve their command ID.
                        // Preserve the permissive recovery probe for old schemas.
                        serde_json::from_slice::<ArchiveRecordProbe>(
                            plain.as_deref().unwrap_or(&line),
                        )
                        .ok()
                        .and_then(|probe| probe.command_id)
                    }
                };
                if let Some(id) = id {
                    archived_offsets
                        .entry(id)
                        .or_default()
                        .push(ArchivedRecordLocation {
                            path: Arc::clone(&archive),
                            offset,
                            len: line.len() as u64,
                            previous,
                            plain_allowed,
                        });
                }
            }
            offset += line.len() as u64;
            line.clear();
        }
        archives.push(archive);
    }
    Ok(WriterState {
        file,
        identity,
        next_sequence,
        file_len: metadata.len(),
        last_digest,
        active_previous: closed.tail,
        sealed_seen,
        active_first_sequence,
        active_records,
        segment_count: closed.count,
        modified: metadata.modified().ok(),
        offsets,
        archives,
        archived_offsets,
        archived_torn_tail,
        archived_incompatible_records,
    })
}

#[async_trait]
impl CommandJournal for JsonlJournal {
    async fn append(&self, record: JournalRecord) -> Result<(), JournalError> {
        let (reply, receiver) = oneshot::channel();
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Pending { record, reply });
        let journal = self.clone();
        // The task outlives a dropped caller, so a queued record is still written.
        tokio::spawn(async move {
            let mut writer = journal.writer.lock().await;
            let batch =
                std::mem::take(&mut *journal.queue.lock().unwrap_or_else(PoisonError::into_inner));
            if !batch.is_empty() {
                journal.commit_batch(&mut writer, batch).await;
            }
        });
        receiver
            .await
            .unwrap_or_else(|_| Err(std::io::Error::other("journal append task ended").into()))
    }

    async fn history(&self, id: CommandId) -> Result<JournalScan, JournalError> {
        let journal = self.clone();
        tokio::spawn(async move {
            let mut writer = journal.writer.lock().await;
            journal.refresh(&mut writer).await?;
            if let Some(offsets) = writer
                .offsets
                .get(&id)
                .filter(|_| !writer.archived_offsets.contains_key(&id))
            {
                // Read the same file that owns the offsets, even if an external
                // replacement occurs after refresh. The next operation refreshes it.
                let damage =
                    match segments::read(&writer.file, offsets, &id, journal.key.as_deref()).await?
                    {
                        segments::Read::Records(records) => {
                            return Ok(JournalScan {
                                records,
                                ..JournalScan::default()
                            });
                        }
                        segments::Read::Damaged(damage) => damage,
                    };
                // In-place damage can leave identity, size and mtime unchanged.
                // Preserve it even when changed bytes still decode as valid JSON.
                match damage {
                    segments::Damage::Active => {
                        let start = Some(writer.active_previous);
                        segments::archive_active(&journal.path, start, journal.key.as_deref())
                            .await?;
                    }
                    segments::Damage::Segment(number) => {
                        segments::archive_from(&journal.path, number, journal.key.as_deref())
                            .await?;
                    }
                }
                *writer = open_writer(
                    &journal.path,
                    journal.max_record_bytes,
                    journal.key.as_deref(),
                )
                .await?;
            }
            let mut scan = JournalScan {
                torn_tail: writer.archived_torn_tail,
                incompatible_records: writer.archived_incompatible_records,
                ..JournalScan::default()
            };
            if let Some(offsets) = writer.archived_offsets.get(&id) {
                // Startup indexes each archive consecutively using one shared path.
                for entries in offsets.chunk_by(|a, b| Arc::ptr_eq(&a.path, &b.path)) {
                    let Some(file) = open_archive(entries[0].path.as_ref()).await? else {
                        scan.incompatible_records += 1;
                        continue;
                    };
                    let mut reader = BufReader::new(file);
                    let mut position = 0;
                    let mut line = Vec::new();
                    for entry in entries {
                        if position != entry.offset {
                            reader.seek(std::io::SeekFrom::Start(entry.offset)).await?;
                            position = entry.offset;
                        }
                        let mut limited = (&mut reader).take(entry.len);
                        line.clear();
                        // Use bytes consumed, since archive damage may end a line early.
                        position += limited.read_until(b'\n', &mut line).await? as u64;
                        if !line.ends_with(b"\n") {
                            scan.torn_tail = true;
                        }
                        let plain = seal::open_at(
                            &line,
                            journal.key.as_deref(),
                            &entry.previous,
                            entry.plain_allowed,
                        );
                        if let Some(Ok(record)) = plain
                            .as_deref()
                            .map(serde_json::from_slice::<JournalRecord>)
                        {
                            if record.command_id == id {
                                scan.records.push(record);
                            } else {
                                // Archive contents may change after offsets are built.
                                // Never attribute another command's diagnostic data.
                                scan.incompatible_records += 1;
                            }
                        }
                    }
                }
            }
            if !writer.archives.is_empty() {
                scan.incompatible_records = scan.incompatible_records.max(1);
            }
            Ok(scan)
        })
        .await
        .map_err(std::io::Error::other)?
    }
}

/// One physical JSONL read. Ledgers decide what a torn or oversized line means.
pub enum JsonlRead {
    Eof,
    /// Includes the trailing newline.
    Complete(Vec<u8>),
    /// Bytes with no trailing newline.
    Torn(Vec<u8>),
    TooLong,
}

/// Discard the rest of an oversized archive line without allocating its body.
async fn discard_jsonl_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<u64> {
    let mut discarded = 0;
    loop {
        let bytes = reader.fill_buf().await?;
        if bytes.is_empty() {
            return Ok(discarded);
        }
        let end = bytes.iter().position(|byte| *byte == b'\n');
        let count = end.map_or(bytes.len(), |end| end + 1);
        reader.consume(count);
        discarded += count as u64;
        if end.is_some() {
            return Ok(discarded);
        }
    }
}

/// Reads one line, capped at `max_bytes`. A longer line is [`JsonlRead::TooLong`]
/// and is not returned.
pub async fn read_jsonl_line<R>(reader: &mut R, max_bytes: u64) -> std::io::Result<JsonlRead>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    read_jsonl_line_buffered(reader, max_bytes, Vec::new()).await
}

async fn read_jsonl_line_buffered<R>(
    reader: &mut R,
    max_bytes: u64,
    mut bytes: Vec<u8>,
) -> std::io::Result<JsonlRead>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    bytes.clear();
    let read = reader
        .take(max_bytes.saturating_add(1))
        .read_until(b'\n', &mut bytes)
        .await?;
    if read == 0 && bytes.is_empty() {
        return Ok(JsonlRead::Eof);
    }
    if bytes.len() as u64 > max_bytes {
        return Ok(JsonlRead::TooLong);
    }
    if bytes.ends_with(b"\n") {
        Ok(JsonlRead::Complete(bytes))
    } else {
        Ok(JsonlRead::Torn(bytes))
    }
}

/// One complete JSONL line, including its trailing newline.
pub struct JsonlLine<'a> {
    pub offset: u64,
    pub number: usize,
    pub bytes: &'a [u8],
}

/// Physical read of a JSONL file. Record rules stay with each ledger.
pub struct JsonlScan {
    pub torn_tail: bool,
    pub bytes_read: u64,
}

/// Visits every complete line. A line with no trailing newline sets `torn_tail`
/// and is not visited. `bytes_read` includes that torn line.
pub async fn for_each_jsonl_line<F>(file: File, visit: F) -> std::io::Result<JsonlScan>
where
    F: FnMut(JsonlLine<'_>) -> std::io::Result<()>,
{
    struct Visitor<F>(F);
    impl<F: FnMut(JsonlLine<'_>) -> std::io::Result<()>> LedgerDecoder for Visitor<F> {
        type Record = ();
        fn decode(&mut self, _line: &JsonlLine<'_>) -> std::io::Result<PreparedRecord<()>> {
            Ok(PreparedRecord {
                observation: RecordObservation::Accepted { sequence: None },
                record: None,
            })
        }
        fn apply(
            &mut self,
            line: &JsonlLine<'_>,
            _prepared: PreparedRecord<()>,
            _verdict: SequenceVerdict,
        ) -> std::io::Result<ScanControl> {
            (self.0)(JsonlLine {
                offset: line.offset,
                number: line.number,
                bytes: line.bytes,
            })?;
            Ok(ScanControl::Continue)
        }
    }
    let health = scan_jsonl(
        &mut BufReader::new(file),
        ScanOptions {
            max_line_bytes: u64::MAX,
            sequence_rule: SequenceRule::None,
        },
        &mut Visitor(visit),
    )
    .await?;
    Ok(JsonlScan {
        torn_tail: health.issue_count(LedgerIssueKind::TornTail) > 0,
        bytes_read: health.bytes_observed,
    })
}

struct CommandDecoder<'a> {
    index: Option<&'a mut HashMap<CommandId, Vec<IndexedRecord>>>,
    opener: LineOpener<'a>,
    /// Digest of the line before the one being decoded.
    line_previous: [u8; 32],
    /// Sequences below this belong to earlier files and are not valid here.
    floor: u64,
    first_sequence: Option<u64>,
}

impl LedgerDecoder for CommandDecoder<'_> {
    type Record = JournalRecord;

    fn decode(&mut self, line: &JsonlLine<'_>) -> std::io::Result<PreparedRecord<JournalRecord>> {
        // Every stored line advances the chain, including skipped ones.
        self.line_previous = self.opener.previous();
        let plain = self.opener.open(line.bytes);
        if line.bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(PreparedRecord {
                observation: RecordObservation::Skip,
                record: None,
            });
        }
        let bytes = plain.as_deref().unwrap_or(line.bytes);
        Ok(match serde_json::from_slice::<JournalRecord>(bytes) {
            Ok(record) if plain.is_some() && record.sequence < self.floor => PreparedRecord {
                observation: RecordObservation::Rejected {
                    sequence_hint: Some(record.sequence),
                    kind: LedgerIssueKind::Sequence,
                },
                record: None,
            },
            Ok(record) if plain.is_some() => PreparedRecord {
                observation: RecordObservation::Accepted {
                    sequence: Some(record.sequence),
                },
                record: Some(record),
            },
            _ => PreparedRecord {
                observation: RecordObservation::Rejected {
                    sequence_hint: serde_json::from_slice::<RecordProbe>(bytes)
                        .ok()
                        .and_then(|probe| probe.sequence),
                    kind: LedgerIssueKind::Decode,
                },
                record: None,
            },
        })
    }

    fn apply(
        &mut self,
        line: &JsonlLine<'_>,
        prepared: PreparedRecord<JournalRecord>,
        verdict: SequenceVerdict,
    ) -> std::io::Result<ScanControl> {
        if let Some(record) = prepared
            .record
            .as_ref()
            .filter(|_| verdict != SequenceVerdict::Invalid)
        {
            self.first_sequence.get_or_insert(record.sequence);
        }
        // Invalid sequence records retain diagnostic identity, never replay authority.
        if let (Some(index), Some(record)) = (self.index.as_deref_mut(), prepared.record) {
            index
                .entry(record.command_id)
                .or_default()
                .push(IndexedRecord::new(
                    line.offset,
                    line.bytes,
                    self.line_previous,
                ));
        }
        Ok(ScanControl::Continue)
    }
}

async fn scan_file(
    file: File,
    index: Option<&mut HashMap<CommandId, Vec<IndexedRecord>>>,
    max_record_bytes: usize,
    key: Option<&JournalKey>,
    start: ScanStart,
) -> Result<Scan, JournalError> {
    let mut decoder = CommandDecoder {
        index,
        opener: LineOpener::resume(key, start.previous, start.sealed_seen),
        line_previous: start.previous,
        floor: start.next_sequence,
        first_sequence: None,
    };
    let health = scan_jsonl(
        &mut BufReader::new(file),
        ScanOptions {
            max_line_bytes: max_record_bytes as u64,
            sequence_rule: SequenceRule::Increasing,
        },
        &mut decoder,
    )
    .await?;
    Ok(Scan {
        scan: JournalScan {
            torn_tail: health.issue_count(LedgerIssueKind::TornTail) > 0,
            incompatible_records: health.incompatible_records
                + health.issue_count(LedgerIssueKind::OversizedLine),
            ..JournalScan::default()
        },
        records: health.decoded_records,
        max_sequence: health.max_observed_sequence,
        last_digest: decoder.opener.previous(),
        sealed_seen: decoder.opener.sealed_seen(),
        first_sequence: decoder.first_sequence,
    })
}
