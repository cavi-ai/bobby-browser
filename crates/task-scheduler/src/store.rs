//! Durable and in-memory job stores.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tracing::warn;

use crate::job::{Job, JobId, JobStatus};

pub const JOURNAL_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("store I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("store serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("journal line {line} is corrupt")]
    Corrupt { line: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JobEvent {
    Submitted,
    Started,
    Completed,
    Failed,
    Retried,
    Cancelled,
    Recovered,
}

impl JobEvent {
    pub fn from_status(status: &JobStatus) -> Self {
        match status {
            JobStatus::Pending => JobEvent::Retried,
            JobStatus::Running => JobEvent::Started,
            JobStatus::Completed => JobEvent::Completed,
            JobStatus::Failed => JobEvent::Failed,
            JobStatus::Cancelled => JobEvent::Cancelled,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JournalRecord {
    pub schema_version: u16,
    pub sequence: u64,
    pub recorded_at: DateTime<Utc>,
    pub event: JobEvent,
    pub job: Job,
}

#[async_trait]
pub trait JobStore: Send + Sync {
    async fn put(&self, job: &Job) -> Result<(), StoreError>;
    async fn get(&self, id: &JobId) -> Result<Option<Job>, StoreError>;
    async fn update(&self, job: &Job, event: JobEvent) -> Result<(), StoreError>;
    async fn pending(&self) -> Result<Vec<Job>, StoreError>;
    async fn load_all(&self) -> Result<Vec<Job>, StoreError>;
}

/// In-memory job index (no durability).
#[derive(Default)]
pub struct MemoryJobStore {
    jobs: Mutex<HashMap<JobId, Job>>,
}

impl MemoryJobStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl JobStore for MemoryJobStore {
    async fn put(&self, job: &Job) -> Result<(), StoreError> {
        let mut jobs = self.jobs.lock().await;
        jobs.insert(job.id.clone(), job.clone());
        Ok(())
    }

    async fn get(&self, id: &JobId) -> Result<Option<Job>, StoreError> {
        let jobs = self.jobs.lock().await;
        Ok(jobs.get(id).cloned())
    }

    async fn update(&self, job: &Job, _event: JobEvent) -> Result<(), StoreError> {
        let mut jobs = self.jobs.lock().await;
        jobs.insert(job.id.clone(), job.clone());
        Ok(())
    }

    async fn pending(&self) -> Result<Vec<Job>, StoreError> {
        let jobs = self.jobs.lock().await;
        Ok(jobs
            .values()
            .filter(|j| j.status == JobStatus::Pending)
            .cloned()
            .collect())
    }

    async fn load_all(&self) -> Result<Vec<Job>, StoreError> {
        let jobs = self.jobs.lock().await;
        Ok(jobs.values().cloned().collect())
    }
}

struct WriterState {
    file: File,
    next_sequence: u64,
}

/// Memory index backed by an append-only JSONL journal.
pub struct JournalJobStore {
    path: Arc<PathBuf>,
    index: MemoryJobStore,
    writer: Mutex<WriterState>,
    recovered_torn_tail: bool,
}

impl JournalJobStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let Scan {
            jobs,
            torn_tail,
            max_sequence,
            incompatible_records,
        } = scan_path(&path).await?;

        if torn_tail {
            truncate_torn_tail(&path).await?;
        }
        if incompatible_records > 0 {
            warn!(
                path = %path.display(),
                incompatible_records,
                schema_version = JOURNAL_SCHEMA_VERSION,
                "skipping journal records written under another schema version"
            );
        }

        let index = MemoryJobStore::new();
        let mut recovered = Vec::new();
        {
            let mut map = index.jobs.lock().await;
            for mut job in jobs {
                // Crash recovery: Running at restart becomes Pending.
                if job.status == JobStatus::Running {
                    job.status = JobStatus::Pending;
                    job.started_at = None;
                    recovered.push(job.clone());
                }
                map.insert(job.id.clone(), job);
            }
        }

        let next_sequence = max_sequence.map_or(0, |sequence| sequence + 1);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)
            .await?;

        let store = Self {
            path: Arc::new(path),
            index,
            writer: Mutex::new(WriterState {
                file,
                next_sequence,
            }),
            recovered_torn_tail: torn_tail,
        };

        for job in &recovered {
            store.append(JobEvent::Recovered, job).await?;
        }

        Ok(store)
    }

    pub fn path(&self) -> &Path {
        self.path.as_ref()
    }

    pub fn recovered_torn_tail(&self) -> bool {
        self.recovered_torn_tail
    }

    async fn append(&self, event: JobEvent, job: &Job) -> Result<(), StoreError> {
        let mut writer = self.writer.lock().await;
        let record = JournalRecord {
            schema_version: JOURNAL_SCHEMA_VERSION,
            sequence: writer.next_sequence,
            recorded_at: Utc::now(),
            event,
            job: job.clone(),
        };
        let mut bytes = serde_json::to_vec(&record)?;
        bytes.push(b'\n');
        writer.file.write_all(&bytes).await?;
        writer.file.flush().await?;
        writer.file.sync_data().await?;
        writer.next_sequence += 1;
        Ok(())
    }
}

#[async_trait]
impl JobStore for JournalJobStore {
    async fn put(&self, job: &Job) -> Result<(), StoreError> {
        self.append(JobEvent::Submitted, job).await?;
        self.index.put(job).await
    }

    async fn get(&self, id: &JobId) -> Result<Option<Job>, StoreError> {
        self.index.get(id).await
    }

    async fn update(&self, job: &Job, event: JobEvent) -> Result<(), StoreError> {
        self.append(event, job).await?;
        self.index.update(job, event).await
    }

    async fn pending(&self) -> Result<Vec<Job>, StoreError> {
        self.index.pending().await
    }

    async fn load_all(&self) -> Result<Vec<Job>, StoreError> {
        self.index.load_all().await
    }
}

struct Scan {
    jobs: Vec<Job>,
    torn_tail: bool,
    max_sequence: Option<u64>,
    incompatible_records: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecordProbe {
    #[serde(default)]
    schema_version: Option<u16>,
}

async fn truncate_torn_tail(path: &Path) -> Result<(), StoreError> {
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

async fn scan_path(path: &Path) -> Result<Scan, StoreError> {
    let mut file = match File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Scan {
                jobs: Vec::new(),
                torn_tail: false,
                max_sequence: None,
                incompatible_records: 0,
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

    let mut latest: HashMap<JobId, Job> = HashMap::new();
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
                if record.schema_version != JOURNAL_SCHEMA_VERSION {
                    incompatible_records += 1;
                    max_sequence = max_sequence.max(Some(record.sequence));
                    continue;
                }
                max_sequence = max_sequence.max(Some(record.sequence));
                latest.insert(record.job.id.clone(), record.job);
            }
            Err(_) => match serde_json::from_slice::<RecordProbe>(line) {
                Ok(probe)
                    if probe
                        .schema_version
                        .is_some_and(|v| v != JOURNAL_SCHEMA_VERSION) =>
                {
                    incompatible_records += 1;
                }
                _ => return Err(StoreError::Corrupt { line: index + 1 }),
            },
        }
    }

    Ok(Scan {
        jobs: latest.into_values().collect(),
        torn_tail,
        max_sequence,
        incompatible_records,
    })
}
