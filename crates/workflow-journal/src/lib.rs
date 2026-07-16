use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use types::{CommandEnvelope, CommandId, CommandOutcome, CommandPhase};

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
}

#[derive(Debug, Clone, Default)]
pub struct JournalScan {
    pub records: Vec<JournalRecord>,
    pub torn_tail: bool,
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

        let scan = scan_path(&path, None).await?;
        if scan.torn_tail {
            truncate_torn_tail(&path).await?;
        }
        let next_sequence = scan
            .records
            .iter()
            .map(|record| record.sequence)
            .max()
            .map_or(0, |sequence| sequence + 1);
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
        let mut scan = scan_path(&self.path, Some(&id)).await?;
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

async fn scan_path(path: &Path, filter: Option<&CommandId>) -> Result<JournalScan, JournalError> {
    let mut file = match File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(JournalScan::default());
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
    for (index, line) in bytes[..complete_len]
        .split(|byte| *byte == b'\n')
        .enumerate()
    {
        if line.is_empty() {
            continue;
        }
        let record: JournalRecord =
            serde_json::from_slice(line).map_err(|_| JournalError::Corrupt { line: index + 1 })?;
        if filter.is_none_or(|id| &record.command_id == id) {
            records.push(record);
        }
    }

    Ok(JournalScan { records, torn_tail })
}
