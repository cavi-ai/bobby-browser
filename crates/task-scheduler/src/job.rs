//! Job definition and state management.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;
use thiserror::Error;

use crate::JobResult;

/// Unique job identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JobId(pub String);

impl JobId {
    pub fn new() -> Self {
        JobId(format!("job_{}", uuid::Uuid::new_v4()))
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

/// Job priority levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
pub enum JobPriority {
    Low,
    Normal,
    High,
    Critical,
}

impl Default for JobPriority {
    fn default() -> Self {
        JobPriority::Normal
    }
}

/// Job execution status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl fmt::Display for JobStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JobStatus::Pending => write!(f, "pending"),
            JobStatus::Running => write!(f, "running"),
            JobStatus::Completed => write!(f, "completed"),
            JobStatus::Failed => write!(f, "failed"),
            JobStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// Job state tracking lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: JobId,
    pub name: String,
    pub priority: JobPriority,
    pub status: JobStatus,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub retry_count: u32,
    pub max_retries: u32,
    pub result: Option<JobResult>,
    pub error: Option<String>,
}

impl Job {
    pub fn new(name: String, payload: serde_json::Value, priority: JobPriority) -> Self {
        Self {
            id: JobId::new(),
            name,
            priority,
            status: JobStatus::Pending,
            payload,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            retry_count: 0,
            max_retries: 3,
            result: None,
            error: None,
        }
    }

    pub fn with_max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    pub fn with_priority(mut self, priority: JobPriority) -> Self {
        self.priority = priority;
        self
    }

    pub fn start(&mut self) {
        self.status = JobStatus::Running;
        self.started_at = Some(Utc::now());
    }

    pub fn complete(&mut self, result: JobResult) {
        self.status = JobStatus::Completed;
        self.completed_at = Some(Utc::now());
        self.result = Some(result);
        self.error = None;
    }

    pub fn fail(&mut self, error: String) {
        self.status = JobStatus::Failed;
        self.completed_at = Some(Utc::now());
        self.error = Some(error);
    }

    pub fn cancel(&mut self) {
        self.status = JobStatus::Cancelled;
        self.completed_at = Some(Utc::now());
    }

    pub fn can_retry(&self) -> bool {
        self.retry_count < self.max_retries
            && (self.status == JobStatus::Failed || self.status == JobStatus::Pending)
    }

    pub fn increment_retry(&mut self) {
        self.retry_count += 1;
    }

    /// Reset a failed job to pending for another attempt.
    pub fn prepare_retry(&mut self) {
        self.retry_count += 1;
        self.status = JobStatus::Pending;
        self.started_at = None;
        self.completed_at = None;
        self.error = None;
        self.result = None;
    }
}

impl fmt::Display for Job {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Job({} [{}] - {})",
            self.id.0, self.status, self.name
        )
    }
}

/// Configuration for a single job.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobConfig {
    pub name: String,
    pub payload: serde_json::Value,
    pub priority: JobPriority,
    pub max_retries: u32,
    pub timeout: Option<Duration>,
}

impl JobConfig {
    pub fn new(name: String, payload: serde_json::Value) -> Self {
        Self {
            name,
            payload,
            priority: JobPriority::default(),
            max_retries: 3,
            timeout: None,
        }
    }

    pub fn with_priority(mut self, priority: JobPriority) -> Self {
        self.priority = priority;
        self
    }

    pub fn with_max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

/// Errors that can occur during job execution.
#[derive(Debug, Error, PartialEq)]
pub enum JobError {
    #[error("job execution timeout after {0:?}")]
    Timeout(Duration),

    #[error("job was cancelled")]
    Cancelled,

    #[error("job failed: {0}")]
    Execution(String),

    #[error("queue is full")]
    QueueFull,

    #[error("concurrency limit reached")]
    ConcurrencyExceeded,

    #[error("job not found: {0}")]
    NotFound(JobId),
}
