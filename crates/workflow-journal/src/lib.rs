use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tracing::warn;
use types::{AttemptId, CommandEnvelope, CommandId, CommandOutcome, CommandPhase, Evidence};

mod health;
pub use health::{
    scan_jsonl, scan_jsonl_prefetched, LedgerDecoder, LedgerHealth, LedgerIssue, LedgerIssueKind,
    PreparedRecord, RecordObservation, ScanControl, ScanOptions, SequenceRule, SequenceVerdict,
};

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
    pub recorded_at: DateTime<Utc>,
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

/// Read-only health of a journal file. Never truncates or creates the path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JournalHealth {
    pub exists: bool,
    pub bytes: u64,
    pub records: usize,
    pub torn_tail: bool,
    pub incompatible_records: usize,
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
}

struct WriterState {
    file: File,
    identity: same_file::Handle,
    next_sequence: u64,
    file_len: u64,
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
}

/// Rebuildable checks against the bytes validated at open or durably appended.
/// Length bounds indexed reads even if a delimiter is subsequently damaged.
struct IndexedRecord {
    offset: u64,
    len: usize,
    digest: [u8; 32],
}

impl IndexedRecord {
    fn new(offset: u64, bytes: &[u8]) -> Self {
        Self {
            offset,
            len: bytes.len(),
            digest: Sha256::digest(bytes).into(),
        }
    }
}

async fn read_indexed_records(
    file: &File,
    entries: &[IndexedRecord],
    id: &CommandId,
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
        let Ok(record) = serde_json::from_slice::<JournalRecord>(&line) else {
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
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("journal has no filename"))?;
    let archive = path.with_file_name(format!(
        "{}.archive-{}",
        name.to_string_lossy(),
        CommandId::new().0
    ));
    tokio::fs::rename(path, &archive).await?;
    sync_parent(path).await?;
    // Also visible for library consumers without a tracing subscriber.
    eprintln!("WARNING: damaged journal {} archived intact to {}; starting a fresh journal. Archived operations require reconciliation.", path.display(), archive.display());
    warn!(path = %path.display(), archive = %archive.display(), "damaged journal archived; starting a fresh journal; old operations require reconciliation");
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
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        let writer = open_writer(&path).await?;
        Ok(Self {
            path: Arc::new(path),
            writer: Arc::new(Mutex::new(writer)),
        })
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
        let path = path.as_ref();
        let file = match File::open(path).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(JournalHealth::default())
            }
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata().await?;
        let Scan { scan, records, .. } = scan_file(file, None).await?;
        Ok(JournalHealth {
            exists: true,
            bytes: metadata.len(),
            records,
            torn_tail: scan.torn_tail,
            incompatible_records: scan.incompatible_records,
        })
    }

    async fn refresh(&self, writer: &mut WriterState) -> Result<(), JournalError> {
        match File::open(&*self.path).await {
            Ok(file) => {
                let metadata = file.metadata().await?;
                let identity = file_identity(file).await?;
                // Size and mtime may survive atomic replacement. The index and
                // append handle must still refer to the file at the current path.
                if identity == writer.identity
                    && metadata.len() == writer.file_len
                    && metadata.modified().ok() == writer.modified
                {
                    return Ok(());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        *writer = open_writer(&self.path).await?;
        Ok(())
    }
}

async fn file_identity(file: File) -> Result<same_file::Handle, std::io::Error> {
    let file = file.into_std().await;
    // Identity uses device/inode on Unix and volume/file index on Windows.
    // Keep its filesystem metadata call off the async runtime threads.
    tokio::task::spawn_blocking(move || same_file::Handle::from_file(file))
        .await
        .map_err(std::io::Error::other)?
}

async fn open_writer(path: &Path) -> Result<WriterState, JournalError> {
    let mut options = OpenOptions::new();
    options.create(true).append(true).read(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).await?;
    let mut offsets = HashMap::<CommandId, Vec<IndexedRecord>>::new();
    // Validate and index the writer's actual file in one streaming pass.
    let scan = scan_file(file.try_clone().await?, Some(&mut offsets)).await?;
    let next_sequence = if scan.scan.torn_tail
        || scan.scan.incompatible_records > 0
        || scan.max_sequence == Some(u64::MAX)
    {
        archive_damaged_journal(path).await?;
        offsets.clear();
        file = options.open(path).await?;
        0
    } else {
        scan.max_sequence.map_or(0, |sequence| sequence + 1)
    };
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
        if !entry.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
        let archive = Arc::new(entry.path());
        let mut reader = BufReader::new(File::open(archive.as_ref()).await?);
        let mut line = Vec::new();
        let mut offset = 0;
        while reader.read_until(b'\n', &mut line).await? > 0 {
            if !line.ends_with(b"\n") {
                archived_torn_tail = true;
            }
            if !line.iter().all(u8::is_ascii_whitespace) {
                let id = match serde_json::from_slice::<JournalRecord>(&line) {
                    Ok(record) => Some(record.command_id),
                    Err(_) => {
                        archived_incompatible_records += 1;
                        // Incompatible records still reserve their command ID.
                        // Preserve the permissive recovery probe for old schemas.
                        serde_json::from_slice::<ArchiveRecordProbe>(&line)
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
    async fn append(&self, mut record: JournalRecord) -> Result<(), JournalError> {
        let journal = self.clone();
        tokio::spawn(async move {
            let mut writer = journal.writer.lock().await;
            journal.refresh(&mut writer).await?;
            if writer.archived_offsets.contains_key(&record.command_id) {
                return Err(JournalError::UncertainCommand);
            }
            let next = writer
                .next_sequence
                .checked_add(1)
                .ok_or(JournalError::UncertainCommand)?;
            record.sequence = writer.next_sequence;
            let mut bytes = serde_json::to_vec(&record)?;
            bytes.push(b'\n');
            writer.file.write_all(&bytes).await?;
            writer.file.flush().await?;
            writer.file.sync_data().await?;
            let offset = writer.file_len;
            writer
                .offsets
                .entry(record.command_id)
                .or_default()
                .push(IndexedRecord::new(offset, &bytes));
            writer.file_len += bytes.len() as u64;
            writer.modified = writer.file.metadata().await?.modified().ok();
            writer.next_sequence = next;
            Ok(())
        })
        .await
        .map_err(std::io::Error::other)?
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
                if let Some(records) = read_indexed_records(&writer.file, offsets, &id).await? {
                    return Ok(JournalScan {
                        records,
                        ..JournalScan::default()
                    });
                }
                // In-place damage can leave identity, size and mtime unchanged.
                // Preserve it even when changed bytes still decode as valid JSON.
                archive_damaged_journal(&journal.path).await?;
                *writer = open_writer(&journal.path).await?;
            }
            let mut scan = JournalScan {
                torn_tail: writer.archived_torn_tail,
                incompatible_records: writer.archived_incompatible_records,
                ..JournalScan::default()
            };
            if let Some(offsets) = writer.archived_offsets.get(&id) {
                // Startup indexes each archive consecutively using one shared path.
                for entries in offsets.chunk_by(|a, b| Arc::ptr_eq(&a.path, &b.path)) {
                    let file = File::open(entries[0].path.as_ref()).await?;
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
                        if let Ok(record) = serde_json::from_slice::<JournalRecord>(&line) {
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
}

impl LedgerDecoder for CommandDecoder<'_> {
    type Record = JournalRecord;

    fn decode(&mut self, line: &JsonlLine<'_>) -> std::io::Result<PreparedRecord<JournalRecord>> {
        if line.bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(PreparedRecord {
                observation: RecordObservation::Skip,
                record: None,
            });
        }
        Ok(match serde_json::from_slice::<JournalRecord>(line.bytes) {
            Ok(record) => PreparedRecord {
                observation: RecordObservation::Accepted {
                    sequence: Some(record.sequence),
                },
                record: Some(record),
            },
            Err(_) => PreparedRecord {
                observation: RecordObservation::Rejected {
                    sequence_hint: serde_json::from_slice::<RecordProbe>(line.bytes)
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
        _verdict: SequenceVerdict,
    ) -> std::io::Result<ScanControl> {
        // Invalid sequence records retain diagnostic identity, never replay authority.
        if let (Some(index), Some(record)) = (self.index.as_deref_mut(), prepared.record) {
            index
                .entry(record.command_id)
                .or_default()
                .push(IndexedRecord::new(line.offset, line.bytes));
        }
        Ok(ScanControl::Continue)
    }
}

async fn scan_file(
    file: File,
    index: Option<&mut HashMap<CommandId, Vec<IndexedRecord>>>,
) -> Result<Scan, JournalError> {
    let health = scan_jsonl(
        &mut BufReader::new(file),
        ScanOptions {
            max_line_bytes: u64::MAX,
            sequence_rule: SequenceRule::Increasing,
        },
        &mut CommandDecoder { index },
    )
    .await?;
    Ok(Scan {
        scan: JournalScan {
            torn_tail: health.issue_count(LedgerIssueKind::TornTail) > 0,
            incompatible_records: health.incompatible_records,
            ..JournalScan::default()
        },
        records: health.decoded_records,
        max_sequence: health.max_observed_sequence,
    })
}
