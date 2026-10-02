use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
    /// Lines skipped because they declare a `CommandEnvelope::SCHEMA_VERSION`
    /// this build does not decode.
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
    /// Highest sequence in the file, including skipped lines, so appends stay monotonic.
    max_sequence: Option<u64>,
}

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("journal I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("journal serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Clone)]
pub struct JsonlJournal {
    path: Arc<PathBuf>,
    writer: Arc<Mutex<WriterState>>,
    recovered_torn_tail: bool,
    history_cache: Arc<Mutex<HistoryCache>>,
}

const HOT_HISTORY_LIMIT: usize = 128;

#[derive(Default)]
struct HistoryCache {
    entries: HashMap<CommandId, (JournalScan, u64, u64)>,
    sequence: u64,
}

impl HistoryCache {
    fn remember(&mut self, id: CommandId, scan: JournalScan, file_len: u64) {
        self.sequence = self.sequence.wrapping_add(1);
        if self.entries.len() >= HOT_HISTORY_LIMIT && !self.entries.contains_key(&id) {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, _, used))| *used)
                .map(|(id, _)| id.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(id, (scan, file_len, self.sequence));
    }
}

struct WriterState {
    file: File,
    next_sequence: u64,
}

impl JsonlJournal {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, JournalError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let Scan { scan, max_sequence } = scan_path(&path, None).await?;
        if scan.torn_tail {
            truncate_torn_tail(&path).await?;
        }
        if scan.incompatible_records > 0 {
            warn!(
                path = %path.display(),
                incompatible_records = scan.incompatible_records,
                schema_version = CommandEnvelope::SCHEMA_VERSION,
                "skipping journal records written under another command schema version"
            );
        }
        let next_sequence = max_sequence.map_or(0, |sequence| sequence + 1);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)
            .await?;

        Ok(Self {
            path: Arc::new(path),
            writer: Arc::new(Mutex::new(WriterState {
                file,
                next_sequence,
            })),
            recovered_torn_tail: scan.torn_tail,
            history_cache: Arc::new(Mutex::new(HistoryCache::default())),
        })
    }

    /// Probe journal health without truncating a torn tail or creating the file.
    pub async fn inspect(path: impl AsRef<Path>) -> Result<JournalHealth, JournalError> {
        let path = path.as_ref();
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(JournalHealth::default());
            }
            Err(error) => return Err(error.into()),
        };
        let bytes_len = bytes.len() as u64;
        let Scan { scan, .. } = scan_bytes(&bytes, None)?;
        Ok(JournalHealth {
            exists: true,
            bytes: bytes_len,
            records: scan.records.len(),
            torn_tail: scan.torn_tail,
            incompatible_records: scan.incompatible_records,
        })
    }
}

#[async_trait]
impl CommandJournal for JsonlJournal {
    async fn append(&self, mut record: JournalRecord) -> Result<(), JournalError> {
        let mut writer = self.writer.lock().await;
        record.sequence = writer.next_sequence;
        let mut bytes = serde_json::to_vec(&record)?;
        bytes.push(b'\n');
        writer.file.write_all(&bytes).await?;
        writer.file.flush().await?;
        writer.file.sync_data().await?;
        writer.next_sequence += 1;
        let Ok(metadata) = writer.file.metadata().await else {
            self.history_cache.lock().await.entries.clear();
            return Ok(());
        };
        let file_len = metadata.len();
        let mut cache = self.history_cache.lock().await;
        let previous_len = file_len.saturating_sub(bytes.len() as u64);
        if cache
            .entries
            .values()
            .any(|(_, len, _)| *len != previous_len)
        {
            cache.entries.clear();
        } else {
            for (id, (scan, len, _)) in &mut cache.entries {
                if *id == record.command_id {
                    scan.records.push(record.clone());
                }
                *len = file_len;
            }
        }
        Ok(())
    }

    async fn history(&self, id: CommandId) -> Result<JournalScan, JournalError> {
        let file_len = tokio::fs::metadata(&*self.path).await?.len();
        let mut cache = self.history_cache.lock().await;
        if let Some((scan, cached_len, _)) = cache.entries.get(&id) {
            if *cached_len == file_len {
                return Ok(scan.clone());
            }
        }
        let mut scan = scan_path(&self.path, Some(&id)).await?.scan;
        scan.torn_tail |= self.recovered_torn_tail;
        cache.remember(id, scan.clone(), file_len);
        Ok(scan)
    }
}

async fn truncate_torn_tail(path: &Path) -> Result<(), JournalError> {
    let bytes = tokio::fs::read(path).await?;
    let complete_len = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |at| at + 1);
    let file = OpenOptions::new().write(true).open(path).await?;
    file.set_len(complete_len as u64).await?;
    file.sync_data().await?;
    Ok(())
}

async fn scan_path(path: &Path, filter: Option<&CommandId>) -> Result<Scan, JournalError> {
    let mut file = match File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Scan {
                scan: JournalScan::default(),
                max_sequence: None,
            });
        }
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await?;
    scan_bytes(&bytes, filter)
}

fn scan_bytes(bytes: &[u8], filter: Option<&CommandId>) -> Result<Scan, JournalError> {
    let torn_tail = !bytes.is_empty() && !bytes.ends_with(b"\n");
    let complete_len = if torn_tail {
        bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |at| at + 1)
    } else {
        bytes.len()
    };

    let mut records = Vec::new();
    let mut incompatible_records = 0;
    let mut max_sequence = None;
    for (index, line) in bytes[..complete_len]
        .split(|byte| *byte == b'\n')
        .enumerate()
    {
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<JournalRecord>(line) {
            Ok(record) => {
                max_sequence = max_sequence.max(Some(record.sequence));
                if filter.is_none_or(|id| &record.command_id == id) {
                    records.push(record);
                }
            }
            Err(_) => {
                // A line this build cannot decode (another schema version, a
                // record shape that changed, or damage) is skipped, so the
                // journal never stops the runtime from starting. A readable
                // sequence still keeps appends monotonic.
                if let Ok(probe) = serde_json::from_slice::<RecordProbe>(line) {
                    max_sequence = max_sequence.max(probe.sequence);
                }
                tracing::warn!(
                    line = index + 1,
                    "command journal line unreadable by this build; skipped"
                );
                incompatible_records += 1;
            }
        }
    }

    Ok(Scan {
        scan: JournalScan {
            records,
            torn_tail,
            incompatible_records,
        },
        max_sequence,
    })
}
