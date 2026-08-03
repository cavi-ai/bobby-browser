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

/// Enough of a journal line to classify one this build cannot decode.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecordProbe {
    #[serde(default)]
    sequence: Option<u64>,
    #[serde(default)]
    envelope: Option<EnvelopeProbe>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnvelopeProbe {
    schema_version: u16,
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
    #[error("journal line {line} is corrupt")]
    Corrupt { line: usize },
}

#[derive(Clone)]
pub struct JsonlJournal {
    path: Arc<PathBuf>,
    writer: Arc<Mutex<WriterState>>,
    recovered_torn_tail: bool,
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
        Ok(())
    }

    async fn history(&self, id: CommandId) -> Result<JournalScan, JournalError> {
        let mut scan = scan_path(&self.path, Some(&id)).await?.scan;
        scan.torn_tail |= self.recovered_torn_tail;
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
                // An undecodable line is tolerated only when it declares a schema
                // version other than this build's. Current-version lines, and lines
                // with no envelope version, are corruption and stay fatal.
                let probe = serde_json::from_slice::<RecordProbe>(line)
                    .map_err(|_| JournalError::Corrupt { line: index + 1 })?;
                let schema_version = probe.envelope.map(|envelope| envelope.schema_version);
                if schema_version.is_none_or(|version| version == CommandEnvelope::SCHEMA_VERSION) {
                    return Err(JournalError::Corrupt { line: index + 1 });
                }
                max_sequence = max_sequence.max(probe.sequence);
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
