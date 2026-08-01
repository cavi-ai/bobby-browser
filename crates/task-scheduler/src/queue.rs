//! Priority-based job queue with retry logic.

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
        // Exponential backoff with jitter
        let exponential = base * (2.0_f64.powi(retry_count as i32));
        let backoff_ms = exponential.min(max);
        // Add jitter (0-20% of backoff)
        let jitter = backoff_ms * 0.2 * (retry_count as f64 * 7.0 % 1.0);
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

    /// Submit a job to the queue.
    pub fn submit(&mut self, config: JobConfig) -> Result<JobId, crate::JobError> {
        if self.jobs.len() >= self.max_size {
            return Err(crate::JobError::QueueFull);
        }

        let mut job = Job::new(config.name, config.payload, config.priority)
            .with_max_retries(config.max_retries);

        if let Some(timeout) = config.timeout {
            // Timeout is handled by the scheduler, store as metadata
            job.payload
                .as_object_mut()
                .map(|obj| {
                    obj.insert("timeout_ms".to_string(), serde_json::json!(timeout.as_millis()));
                });
        }

        let id = job.id.clone();
        self.jobs.push_back(job);
        Ok(id)
    }

    /// Get the next job to execute (highest priority, FIFO within priority).
    pub fn next_job(&mut self) -> Option<Job> {
        if self.jobs.is_empty() {
            return None;
        }

        // Find the highest priority job
        let mut best_idx = 0;
        let mut best_priority = self.jobs[0].priority.clone();

        for (i, job) in self.jobs.iter().enumerate().skip(1) {
            if job.priority > best_priority {
                best_idx = i;
                best_priority = job.priority.clone();
            }
        }

        Some(self.jobs.remove(best_idx).unwrap())
    }

    /// Complete a job and optionally requeue it for retry.
    pub fn complete_job(
        &mut self,
        job_id: &JobId,
        result: JobResult,
    ) -> Result<(), crate::JobError> {
        if let Some(job) = self.jobs.iter_mut().find(|j| j.id == *job_id) {
            job.clone().complete(result);
            Ok(())
        } else {
            Err(crate::JobError::Execution(
                "job not found in queue".to_string(),
            ))
        }
    }

    /// Mark a job for retry.
    pub fn retry_job(&mut self, job_id: &JobId) -> Result<Duration, crate::JobError> {
        if let Some(job) = self.jobs.iter_mut().find(|j| j.id == *job_id) {
            if job.can_retry() {
                job.increment_retry();
                job.status = JobStatus::Pending;
                let backoff = self.retry_config.calculate_backoff(job.retry_count);
                Ok(backoff)
            } else {
                let _failed = job.clone().fail("max retries exceeded".to_string());
                Err(crate::JobError::Execution("max retries exceeded".to_string()))
            }
        } else {
            Err(crate::JobError::Execution(
                "job not found in queue".to_string(),
            ))
        }
    }

    /// Cancel a job by ID.
    pub fn cancel_job(&mut self, job_id: &JobId) -> Result<(), crate::JobError> {
        if let Some(job) = self.jobs.iter_mut().find(|j| j.id == *job_id) {
            job.clone().cancel();
            Ok(())
        } else {
            Err(crate::JobError::Execution(
                "job not found in queue".to_string(),
            ))
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

    /// Get queue statistics.
    pub fn stats(&self) -> QueueStats {
        let mut pending = 0;
        let mut running = 0;
        let mut failed = 0;
        let mut completed = 0;

        for job in &self.jobs {
            match job.status {
                JobStatus::Pending => pending += 1,
                JobStatus::Running => running += 1,
                JobStatus::Completed => completed += 1,
                JobStatus::Failed => failed += 1,
                JobStatus::Cancelled => {}
            }
        }

        QueueStats {
            total: self.jobs.len(),
            pending,
            running,
            completed,
            failed,
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
}
