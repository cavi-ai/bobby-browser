//! Task scheduler for browser automation with job queues, retries, and concurrency limits.
//!
//! Provides a priority-based job queue with exponential backoff retry logic,
//! configurable concurrency limits, and job lifecycle tracking.

mod job;
mod queue;
mod scheduler;

pub use job::{Job, JobConfig, JobError, JobId, JobPriority, JobStatus};
pub use queue::{JobQueue, RetryConfig};
pub use scheduler::JobScheduler;

use serde::{Deserialize, Serialize};

/// Configuration for the task scheduler.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SchedulerConfig {
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_jobs: usize,
    #[serde(default = "default_queue_size")]
    pub max_queue_size: usize,
    #[serde(default = "default_retry_max")]
    pub max_retries: u32,
    #[serde(default = "default_retry_base_ms")]
    pub retry_backoff_base_ms: u64,
    #[serde(default = "default_retry_max_ms")]
    pub retry_backoff_max_ms: u64,
    #[serde(default = "default_job_timeout_ms")]
    pub job_timeout_ms: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            max_concurrent_jobs: default_max_concurrent(),
            max_queue_size: default_queue_size(),
            max_retries: default_retry_max(),
            retry_backoff_base_ms: default_retry_base_ms(),
            retry_backoff_max_ms: default_retry_max_ms(),
            job_timeout_ms: default_job_timeout_ms(),
        }
    }
}

fn default_max_concurrent() -> usize {
    10
}

fn default_queue_size() -> usize {
    1000
}

fn default_retry_max() -> u32 {
    3
}

fn default_retry_base_ms() -> u64 {
    1000
}

fn default_retry_max_ms() -> u64 {
    60000
}

fn default_job_timeout_ms() -> u64 {
    300000 // 5 minutes
}

impl SchedulerConfig {
    pub fn with_max_concurrent(mut self, count: usize) -> Self {
        self.max_concurrent_jobs = count;
        self
    }

    pub fn with_queue_size(mut self, size: usize) -> Self {
        self.max_queue_size = size;
        self
    }

    pub fn with_max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    pub fn with_backoff_range(mut self, base_ms: u64, max_ms: u64) -> Self {
        self.retry_backoff_base_ms = base_ms;
        self.retry_backoff_max_ms = max_ms;
        self
    }

    pub fn with_job_timeout(mut self, timeout_ms: u64) -> Self {
        self.job_timeout_ms = timeout_ms;
        self
    }
}

/// Result of a job execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobResult {
    pub job_id: JobId,
    pub success: bool,
    pub output: Option<serde_json::Value>,
    pub error: Option<String>,
    pub completed_at: chrono::DateTime<chrono::Utc>,
}

/// Statistics about the scheduler.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchedulerStats {
    pub total_submitted: u64,
    pub total_completed: u64,
    pub total_failed: u64,
    pub total_retried: u64,
    pub active_jobs: usize,
    pub queued_jobs: usize,
}
