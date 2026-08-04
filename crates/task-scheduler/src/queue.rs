//! Priority-based job queue with retry logic.

use rand::RngExt;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::Duration;

use crate::job::{Job, JobConfig, JobId, JobStatus};
use crate::JobResult;

/// Configuration for retry behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryConfig {
    pub max_retries: u32,
    pub backoff_base_ms: u64,
    pub backoff_max_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            backoff_base_ms: 1000,
            backoff_max_ms: 60000,
        }
    }
}

impl RetryConfig {
    pub fn calculate_backoff(&self, retry_count: u32) -> Duration {
        let base = self.backoff_base_ms as f64;
        let max = self.backoff_max_ms as f64;
        let exponential = base * (2.0_f64.powi(retry_count as i32));
        let backoff_ms = exponential.min(max);
        // Jitter: 0-20% of capped backoff
        let jitter_factor: f64 = rand::rng().random();
        let jitter = backoff_ms * 0.2 * jitter_factor;
        Duration::from_millis((backoff_ms + jitter) as u64)
    }
}

/// Priority-based job queue.
pub struct JobQueue {
    jobs: VecDeque<Job>,
    max_size: usize,
    retry_config: RetryConfig,
}

impl JobQueue {
    pub fn new(max_size: usize) -> Self {
        Self {
            jobs: VecDeque::with_capacity(max_size),
            max_size,
            retry_config: RetryConfig::default(),
        }
    }

    pub fn with_retry_config(mut self, config: RetryConfig) -> Self {
        self.retry_config = config;
        self
    }

    pub fn retry_config(&self) -> &RetryConfig {
        &self.retry_config
    }

    /// Submit a job to the queue.
    pub fn submit(&mut self, config: JobConfig) -> Result<Job, crate::JobError> {
        if self.jobs.len() >= self.max_size {
            return Err(crate::JobError::QueueFull);
        }

        let mut job = Job::new(config.name, config.payload, config.priority)
            .with_max_retries(config.max_retries);

        if let Some(timeout) = config.timeout {
            job.timeout_ms = Some(timeout.as_millis() as u64);
        }
        if let Some(correlation_id) = config.correlation_id {
            job.correlation_id = Some(correlation_id);
        }

        self.jobs.push_back(job.clone());
        Ok(job)
    }

    /// Re-queue an existing job (same id) after a retry delay.
    pub fn requeue(&mut self, job: Job) -> Result<(), crate::JobError> {
        if self.jobs.len() >= self.max_size {
            return Err(crate::JobError::QueueFull);
        }
        self.jobs.push_back(job);
        Ok(())
    }

    /// Get the next pending job (highest priority, FIFO within priority).
    pub fn next_job(&mut self) -> Option<Job> {
        let best_idx = self
            .jobs
            .iter()
            .enumerate()
            .filter(|(_, job)| job.status == JobStatus::Pending)
            .max_by(|(ia, a), (ib, b)| {
                // Higher priority wins; on ties, earlier index (FIFO) wins.
                a.priority.cmp(&b.priority).then_with(|| ib.cmp(ia))
            })
            .map(|(i, _)| i)?;

        let mut job = self.jobs.remove(best_idx).unwrap();
        job.start();
        Some(job)
    }

    /// Complete a job still present in the queue.
    pub fn complete_job(
        &mut self,
        job_id: &JobId,
        result: JobResult,
    ) -> Result<(), crate::JobError> {
        if let Some(job) = self.jobs.iter_mut().find(|j| j.id == *job_id) {
            job.complete(result);
            Ok(())
        } else {
            Err(crate::JobError::NotFound(job_id.clone()))
        }
    }

    /// Mark a queued job for retry and return backoff duration.
    pub fn retry_job(&mut self, job_id: &JobId) -> Result<Duration, crate::JobError> {
        if let Some(job) = self.jobs.iter_mut().find(|j| j.id == *job_id) {
            if job.can_retry() {
                job.prepare_retry();
                Ok(self.retry_config.calculate_backoff(job.retry_count))
            } else {
                job.fail("max retries exceeded".to_string());
                Err(crate::JobError::Execution(
                    "max retries exceeded".to_string(),
                ))
            }
        } else {
            Err(crate::JobError::NotFound(job_id.clone()))
        }
    }

    /// Cancel a job by ID. Removes it from the pending queue.
    pub fn cancel_job(&mut self, job_id: &JobId) -> Result<Job, crate::JobError> {
        if let Some(idx) = self.jobs.iter().position(|j| j.id == *job_id) {
            let mut job = self.jobs.remove(idx).unwrap();
            if matches!(
                job.status,
                JobStatus::Completed | JobStatus::Failed | JobStatus::Cancelled
            ) {
                return Err(crate::JobError::Execution(format!(
                    "job {} already finished with status {}",
                    job_id, job.status
                )));
            }
            job.cancel();
            Ok(job)
        } else {
            Err(crate::JobError::NotFound(job_id.clone()))
        }
    }

    /// Get the number of jobs in the queue.
    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    /// Check if the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    /// Count pending jobs.
    pub fn pending_count(&self) -> usize {
        self.jobs
            .iter()
            .filter(|j| j.status == JobStatus::Pending)
            .count()
    }

    /// Get queue statistics.
    pub fn stats(&self) -> QueueStats {
        let mut pending = 0;
        let mut running = 0;
        let mut failed = 0;
        let mut completed = 0;
        let mut cancelled = 0;

        for job in &self.jobs {
            match job.status {
                JobStatus::Pending => pending += 1,
                JobStatus::Running => running += 1,
                JobStatus::Completed => completed += 1,
                JobStatus::Failed => failed += 1,
                JobStatus::Cancelled => cancelled += 1,
            }
        }

        QueueStats {
            total: self.jobs.len(),
            pending,
            running,
            completed,
            failed,
            cancelled,
        }
    }
}

/// Statistics about the job queue.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueStats {
    pub total: usize,
    pub pending: usize,
    pub running: usize,
    pub completed: usize,
    pub failed: usize,
    pub cancelled: usize,
}
