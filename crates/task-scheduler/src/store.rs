//! Durable and in-memory job stores.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use workflow_journal::{
    JsonlLine, LedgerDecoder, LedgerIssueKind, PreparedRecord, RecordObservation, ScanControl,
    ScanOptions, SequenceRule, SequenceVerdict,
};

use crate::job::{Job, JobId, JobStatus};

pub const JOURNAL_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("job journal integrity requires repair before mutation")]
    Integrity,
    #[error("store I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("store serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

pub use workflow_journal::JournalHealth;

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
    Resolved,
}

impl JobEvent {
    pub fn from_status(status: &JobStatus) -> Self {
        match status {
            JobStatus::Pending => JobEvent::Retried,
            JobStatus::Running => JobEvent::Started,
            JobStatus::Completed => JobEvent::Completed,
            JobStatus::Failed => JobEvent::Failed,
            JobStatus::Cancelled => JobEvent::Cancelled,
            JobStatus::ReconciliationRequired => JobEvent::Recovered,
            JobStatus::Resolved => JobEvent::Resolved,
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
    fn integrity_issue(&self) -> Option<&'static str> {
        None
    }
    async fn put(&self, job: &Job) -> Result<(), StoreError>;
    async fn get(&self, id: &JobId) -> Result<Option<Job>, StoreError>;
    async fn update(&self, job: &Job, event: JobEvent) -> Result<(), StoreError>;
    async fn pending(&self) -> Result<Vec<Job>, StoreError>;
    async fn load_all(&self) -> Result<Vec<Job>, StoreError>;
    /// Drop terminal jobs beyond `retained`, oldest first; pending/running
    /// jobs are never removed. Default no-op for stores without retention.
    async fn prune_terminal(&self, retained: usize) -> Result<(), StoreError> {
        let _ = retained;
        Ok(())
    }
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

    async fn prune_terminal(&self, retained: usize) -> Result<(), StoreError> {
        let mut jobs = self.jobs.lock().await;
        let mut terminal: Vec<(JobId, Option<DateTime<Utc>>)> = jobs
            .iter()
            .filter(|(_, job)| {
                matches!(
                    job.status,
                    JobStatus::Completed
                        | JobStatus::Failed
                        | JobStatus::Cancelled
                        | JobStatus::Resolved
                )
            })
            .map(|(id, job)| (id.clone(), Some(job.completed_at.unwrap_or(job.created_at))))
            .collect();
        let excess = terminal.len().saturating_sub(retained);
        terminal.sort_by_key(|(_, completed_at)| *completed_at);
        for (id, _) in terminal.into_iter().take(excess) {
            jobs.remove(&id);
        }
        Ok(())
    }
}

struct WriterState {
    file: File,
    next_sequence: u64,
}

/// Atomically publish exactly the retained current-state snapshot. The caller
/// owns the writer lock and retention policy; compaction never drops active or
/// uncertain work on its own.
async fn compact_journal<'a>(
    path: &Path,
    jobs: impl Iterator<Item = &'a Job>,
) -> Result<(), StoreError> {
    let temporary = path.with_extension(format!("{}.compact.tmp", uuid::Uuid::new_v4()));
    let result = async {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temporary).await?;
        let mut jobs: Vec<_> = jobs.collect();
        jobs.sort_by_key(|job| job.created_at);
        for (sequence, job) in jobs.into_iter().enumerate() {
            let record = JournalRecord {
                schema_version: JOURNAL_SCHEMA_VERSION,
                sequence: sequence as u64,
                recorded_at: Utc::now(),
                event: JobEvent::from_status(&job.status),
                job: job.clone(),
            };
            let mut bytes = serde_json::to_vec(&record)?;
            bytes.push(b'\n');
            file.write_all(&bytes).await?;
        }
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temporary, path).await?;
        if let Some(parent) = path.parent() {
            File::open(parent).await?.sync_all().await?;
        }
        Ok::<_, StoreError>(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result
}

const COMPACT_THRESHOLD: usize = 4096;
const COMPACT_RETAINED_TERMINAL: usize = 1024;

#[derive(Clone)]
pub struct JournalJobStore {
    path: Arc<PathBuf>,
    index: Arc<MemoryJobStore>,
    writer: Arc<Mutex<WriterState>>,
    recovered_torn_tail: bool,
    integrity_issue: Option<&'static str>,
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
            ..
        } = scan_path(&path, true).await?;

        let damaged = torn_tail || incompatible_records > 0 || max_sequence == Some(u64::MAX);
        if damaged {
            workflow_journal::archive_damaged_journal(&path).await?;
        }
        let index = Arc::new(MemoryJobStore::new());
        let mut recovered = Vec::new();
        {
            let mut map = index.jobs.lock().await;
            for mut job in jobs {
                // A handler may have acted before the process stopped. Never
                // replay an interrupted execution without reconciliation.
                if damaged {
                    job.resolution = None;
                    job.require_reconciliation(
                        "job history was damaged and archived; reconciliation required",
                    );
                } else if job.status == JobStatus::Running {
                    job.require_reconciliation("execution interrupted; outcome must be reconciled");
                    recovered.push(job.clone());
                }
                if job.status == JobStatus::ReconciliationRequired {
                    let reason = job
                        .error
                        .clone()
                        .unwrap_or_else(|| "uncertain execution".into());
                    job.require_reconciliation(reason);
                }
                map.insert(job.id.clone(), job);
            }
        }

        let next_sequence = if damaged {
            let jobs = index.jobs.lock().await;
            compact_journal(&path, jobs.values()).await?;
            jobs.len() as u64
        } else if max_sequence.is_some_and(|sequence| sequence >= COMPACT_THRESHOLD as u64) {
            index.prune_terminal(COMPACT_RETAINED_TERMINAL).await?;
            let jobs = index.jobs.lock().await;
            compact_journal(&path, jobs.values()).await?;
            jobs.len() as u64
        } else {
            max_sequence
                .and_then(|sequence| sequence.checked_add(1))
                .unwrap_or(0)
        };
        let mut options = OpenOptions::new();
        options.create(true).append(true).read(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(&path).await?;

        let store = Self {
            path: Arc::new(path),
            index,
            writer: Arc::new(Mutex::new(WriterState {
                file,
                next_sequence,
            })),
            recovered_torn_tail: torn_tail,
            integrity_issue: None,
        };

        for job in &recovered {
            store.append(JobEvent::Recovered, job).await?;
        }

        Ok(store)
    }

    /// Probe journal health without truncating, compacting, or creating the file.
    pub async fn inspect(path: impl AsRef<Path>) -> Result<JournalHealth, StoreError> {
        let scan = scan_path(path.as_ref(), false).await?;
        Ok(JournalHealth {
            exists: scan.exists,
            bytes: scan.bytes,
            records: scan.records,
            torn_tail: scan.torn_tail,
            incompatible_records: scan.incompatible_records,
        })
    }

    pub fn path(&self) -> &Path {
        self.path.as_ref()
    }

    pub fn recovered_torn_tail(&self) -> bool {
        self.recovered_torn_tail
    }

    async fn append(&self, event: JobEvent, job: &Job) -> Result<(), StoreError> {
        let store = self.clone();
        let job = job.clone();
        // Keep the durable append and memory transition under one owned writer,
        // even when the submitting future is cancelled.
        tokio::spawn(async move {
            let mut writer = store.writer.lock().await;
            let mut jobs = store.index.jobs.lock().await;
            let next = writer
                .next_sequence
                .checked_add(1)
                .ok_or(StoreError::Integrity)?;
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
            writer.next_sequence = next;
            jobs.insert(job.id.clone(), job);
            if writer.next_sequence >= jobs.len() as u64 + COMPACT_THRESHOLD as u64 {
                let result = compact_journal(&store.path, jobs.values()).await;
                // Reopen after any attempted rename, including a directory-sync
                // error, so later appends never target an unlinked old inode.
                writer.file = OpenOptions::new()
                    .append(true)
                    .read(true)
                    .open(&*store.path)
                    .await?;
                writer.next_sequence = scan_path(&store.path, false)
                    .await?
                    .max_sequence
                    .and_then(|seq| seq.checked_add(1))
                    .ok_or(StoreError::Integrity)?;
                result?;
            }
            Ok(())
        })
        .await
        .map_err(std::io::Error::other)?
    }
}

#[async_trait]
impl JobStore for JournalJobStore {
    fn integrity_issue(&self) -> Option<&'static str> {
        self.integrity_issue
    }
    async fn put(&self, job: &Job) -> Result<(), StoreError> {
        let event = if job.status == JobStatus::Pending {
            JobEvent::Submitted
        } else {
            JobEvent::from_status(&job.status)
        };
        self.append(event, job).await
    }

    async fn get(&self, id: &JobId) -> Result<Option<Job>, StoreError> {
        self.index.get(id).await
    }

    async fn update(&self, job: &Job, event: JobEvent) -> Result<(), StoreError> {
        self.append(event, job).await
    }

    async fn pending(&self) -> Result<Vec<Job>, StoreError> {
        self.index.pending().await
    }

    async fn load_all(&self) -> Result<Vec<Job>, StoreError> {
        self.index.load_all().await
    }
    async fn prune_terminal(&self, retained: usize) -> Result<(), StoreError> {
        let store = self.clone();
        tokio::spawn(async move {
            let mut writer = store.writer.lock().await;
            let mut jobs = store.index.jobs.lock().await;
            let mut terminal: Vec<_> = jobs
                .values()
                .filter(|job| {
                    matches!(
                        job.status,
                        JobStatus::Completed
                            | JobStatus::Failed
                            | JobStatus::Cancelled
                            | JobStatus::Resolved
                    )
                })
                .map(|job| (job.id.clone(), job.completed_at.unwrap_or(job.created_at)))
                .collect();
            let excess = terminal.len().saturating_sub(retained);
            if excess == 0 {
                return Ok(());
            }
            terminal.sort_by_key(|(_, at)| *at);
            let mut retained_jobs = jobs.clone();
            for (id, _) in terminal.into_iter().take(excess) {
                retained_jobs.remove(&id);
            }
            let result = compact_journal(&store.path, retained_jobs.values()).await;
            writer.file = OpenOptions::new()
                .append(true)
                .read(true)
                .open(&*store.path)
                .await?;
            let scan = scan_path(&store.path, true).await?;
            writer.next_sequence = scan
                .max_sequence
                .and_then(|seq| seq.checked_add(1))
                .unwrap_or(0);
            *jobs = scan
                .jobs
                .into_iter()
                .map(|job| (job.id.clone(), job))
                .collect();
            result
        })
        .await
        .map_err(std::io::Error::other)?
    }
}

#[derive(Default)]
struct Scan {
    jobs: Vec<Job>,
    torn_tail: bool,
    max_sequence: Option<u64>,
    incompatible_records: usize,
    exists: bool,
    bytes: u64,
    records: usize,
}

async fn scan_path(path: &Path, collect_jobs: bool) -> Result<Scan, StoreError> {
    let file = match File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Scan::default());
        }
        Err(error) => return Err(error.into()),
    };
    let mut decoder = JobDecoder {
        latest: HashMap::new(),
        collect_jobs,
        records: 0,
    };
    let health = workflow_journal::scan_jsonl(
        &mut BufReader::new(file),
        ScanOptions {
            max_line_bytes: u64::MAX,
            sequence_rule: SequenceRule::Increasing,
        },
        &mut decoder,
    )
    .await?;
    Ok(Scan {
        exists: true,
        bytes: health.bytes_observed,
        records: decoder.records,
        max_sequence: health.max_observed_sequence,
        torn_tail: health.issue_count(LedgerIssueKind::TornTail) > 0,
        incompatible_records: health.incompatible_records,
        jobs: decoder.latest.into_values().collect(),
    })
}

struct JobDecoder {
    latest: HashMap<JobId, Job>,
    collect_jobs: bool,
    records: usize,
}

impl LedgerDecoder for JobDecoder {
    type Record = JournalRecord;
    fn decode(&mut self, line: &JsonlLine<'_>) -> std::io::Result<PreparedRecord<JournalRecord>> {
        let body = &line.bytes[..line.bytes.len() - 1];
        if body.is_empty() {
            return Ok(PreparedRecord {
                observation: RecordObservation::Skip,
                record: None,
            });
        }
        self.records += 1;
        let record = match serde_json::from_slice::<JournalRecord>(body) {
            Ok(record) => record,
            Err(_) => {
                tracing::warn!(
                    line = line.number,
                    "job journal line unreadable by this build; skipped"
                );
                return Ok(PreparedRecord {
                    observation: RecordObservation::Rejected {
                        sequence_hint: None,
                        kind: LedgerIssueKind::Decode,
                    },
                    record: None,
                });
            }
        };
        let event_matches_status = record.event == JobEvent::from_status(&record.job.status)
            || (record.event == JobEvent::Submitted && record.job.status == JobStatus::Pending);
        let issue = if record.schema_version != JOURNAL_SCHEMA_VERSION {
            Some(LedgerIssueKind::Schema)
        } else if !record.job.has_valid_resolution() || !event_matches_status {
            Some(LedgerIssueKind::InvalidState)
        } else {
            None
        };
        Ok(PreparedRecord {
            observation: match issue {
                Some(kind) => RecordObservation::Rejected {
                    sequence_hint: Some(record.sequence),
                    kind,
                },
                None => RecordObservation::Accepted {
                    sequence: Some(record.sequence),
                },
            },
            record: Some(record),
        })
    }

    fn apply(
        &mut self,
        _line: &JsonlLine<'_>,
        prepared: PreparedRecord<JournalRecord>,
        verdict: SequenceVerdict,
    ) -> std::io::Result<ScanControl> {
        if self.collect_jobs && verdict == SequenceVerdict::Valid {
            if let Some(record) = prepared.record {
                self.latest.insert(record.job.id.clone(), record.job);
            }
        }
        Ok(ScanControl::Continue)
    }
}
