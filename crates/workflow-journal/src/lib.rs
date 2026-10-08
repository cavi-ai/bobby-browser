use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tracing::warn;
use types::{AttemptId, CommandEnvelope, CommandId, CommandOutcome, CommandPhase, Evidence};

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
    next_sequence: u64,
    file_len: u64,
    modified: Option<std::time::SystemTime>,
    offsets: HashMap<CommandId, Vec<u64>>,
    archives: Vec<PathBuf>,
    archived_offsets: HashMap<CommandId, Vec<(PathBuf, u64)>>,
    archived_torn_tail: bool,
    archived_incompatible_records: usize,
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
        self.writer.lock().await.archives.clone()
    }

    pub async fn inspect(path: impl AsRef<Path>) -> Result<JournalHealth, JournalError> {
        let path = path.as_ref();
        let metadata = match tokio::fs::metadata(path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(JournalHealth::default())
            }
            Err(error) => return Err(error.into()),
        };
        let Scan { scan, records, .. } = scan_path(path).await?;
        Ok(JournalHealth {
            exists: true,
            bytes: metadata.len(),
            records,
            torn_tail: scan.torn_tail,
            incompatible_records: scan.incompatible_records,
        })
    }

    async fn refresh(&self, writer: &mut WriterState) -> Result<(), JournalError> {
        let metadata = tokio::fs::metadata(&*self.path).await;
        if metadata
            .as_ref()
            .is_ok_and(|m| m.len() == writer.file_len && m.modified().ok() == writer.modified)
        {
            return Ok(());
        }
        *writer = open_writer(&self.path).await?;
        Ok(())
    }
}

async fn open_writer(path: &Path) -> Result<WriterState, JournalError> {
    let scan = scan_path(path).await?;
    if scan.scan.torn_tail
        || scan.scan.incompatible_records > 0
        || scan.max_sequence == Some(u64::MAX)
    {
        archive_damaged_journal(path).await?;
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true).read(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(path).await?;
    file.sync_all().await?;
    sync_parent(path).await?;
    let metadata = file.metadata().await?;
    let mut offsets = HashMap::<CommandId, Vec<u64>>::new();
    let mut reader = BufReader::new(File::open(path).await?);
    let mut offset = 0;
    let mut next_sequence = 0;
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line).await? > 0 {
        if !line.iter().all(u8::is_ascii_whitespace) {
            let record: JournalRecord = serde_json::from_slice(&line)?;
            offsets.entry(record.command_id).or_default().push(offset);
            next_sequence = record
                .sequence
                .checked_add(1)
                .ok_or(JournalError::UncertainCommand)?;
        }
        offset += line.len() as u64;
        line.clear();
    }
    let mut archives = Vec::new();
    let mut archived_offsets = HashMap::<CommandId, Vec<(PathBuf, u64)>>::new();
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
        let archive = entry.path();
        let mut reader = BufReader::new(File::open(&archive).await?);
        let mut line = Vec::new();
        let mut offset = 0;
        while reader.read_until(b'\n', &mut line).await? > 0 {
            if !line.ends_with(b"\n") {
                archived_torn_tail = true;
            }
            if !line.iter().all(u8::is_ascii_whitespace) {
                if serde_json::from_slice::<JournalRecord>(&line).is_err() {
                    archived_incompatible_records += 1;
                }
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&line) {
                    if let Some(id) = value
                        .get("commandId")
                        .and_then(|v| serde_json::from_value(v.clone()).ok())
                    {
                        archived_offsets
                            .entry(id)
                            .or_default()
                            .push((archive.clone(), offset));
                    }
                }
            }
            offset += line.len() as u64;
            line.clear();
        }
        archives.push(archive);
    }
    Ok(WriterState {
        file,
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
                .push(offset);
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
                let mut reader = BufReader::new(File::open(&*journal.path).await?);
                let mut records = Vec::with_capacity(offsets.len());
                for offset in offsets {
                    reader.seek(std::io::SeekFrom::Start(*offset)).await?;
                    let mut line = Vec::new();
                    reader.read_until(b'\n', &mut line).await?;
                    records.push(serde_json::from_slice(&line)?);
                }
                return Ok(JournalScan {
                    records,
                    ..JournalScan::default()
                });
            }
            let mut scan = JournalScan {
                torn_tail: writer.archived_torn_tail,
                incompatible_records: writer.archived_incompatible_records,
                ..JournalScan::default()
            };
            if let Some(offsets) = writer.archived_offsets.get(&id) {
                for (path, offset) in offsets {
                    let mut reader = BufReader::new(File::open(path).await?);
                    reader.seek(std::io::SeekFrom::Start(*offset)).await?;
                    let mut line = Vec::new();
                    reader.read_until(b'\n', &mut line).await?;
                    if let Ok(record) = serde_json::from_slice(&line) {
                        scan.records.push(record);
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

async fn scan_path(path: &Path) -> Result<Scan, JournalError> {
    let file = match File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Scan {
                scan: JournalScan::default(),
                records: 0,
                max_sequence: None,
            })
        }
        Err(error) => return Err(error.into()),
    };
    let mut reader = BufReader::new(file);
    let mut scan = JournalScan::default();
    let mut records = 0usize;
    let mut max_sequence = None;
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line).await? > 0 {
        if !line.ends_with(b"\n") {
            scan.torn_tail = true;
            break;
        }
        if !line.iter().all(u8::is_ascii_whitespace) {
            match serde_json::from_slice::<JournalRecord>(&line) {
                Ok(record) => {
                    records += 1;
                    if max_sequence.is_some_and(|sequence| record.sequence <= sequence) {
                        scan.incompatible_records += 1;
                    }
                    max_sequence = max_sequence.max(Some(record.sequence));
                }
                Err(_) => {
                    if let Ok(probe) = serde_json::from_slice::<RecordProbe>(&line) {
                        max_sequence = max_sequence.max(probe.sequence);
                    }
                    scan.incompatible_records += 1;
                }
            }
        }
        line.clear();
    }
    Ok(Scan {
        scan,
        records,
        max_sequence,
    })
}
