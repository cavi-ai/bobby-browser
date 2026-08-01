//! Job scheduler with concurrency control and retry logic.

use async_trait::async_trait;
use chrono::Utc;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore};
use tokio::time::timeout;
use tracing::{error, info, warn};

use crate::job::{Job, JobConfig, JobId, JobStatus};
use crate::queue::JobQueue;
use crate::{JobResult, SchedulerConfig, SchedulerStats};

/// Trait for job execution handlers.
#[async_trait]
pub trait JobHandler: Send + Sync {
    /// Execute a job and return the result.
    async fn execute(&self, job: &Job) -> Result<serde_json::Value, String>;
}

/// Job scheduler with concurrency control.
pub struct JobScheduler {
    config: SchedulerConfig,
    queue: Mutex<JobQueue>,
    semaphore: Arc<Semaphore>,
    handlers: HashMap<String, Arc<dyn JobHandler>>,
    job_registry: Mutex<HashMap<JobId, Job>>,
}

impl JobScheduler {
    pub fn new(config: SchedulerConfig) -> Self {
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent_jobs));
        Self {
            config: config.clone(),
            queue: Mutex::new(JobQueue::new(config.max_queue_size)),
            semaphore,
            handlers: HashMap::new(),
            job_registry: Mutex::new(HashMap::new()),
        }
    }

    /// Register a job handler by name.
    pub fn register_handler(&mut self, name: String, handler: Arc<dyn JobHandler>) {
        self.handlers.insert(name, handler);
    }

    /// Submit a job to the scheduler.
    pub async fn submit(&self, config: JobConfig) -> Result<JobId, crate::JobError> {
        let mut queue = self.queue.lock().await;
        let id = queue.submit(config)?;
        Ok(id)
    }

    /// Start processing jobs from the queue.
    pub async fn run(&self) -> Result<(), crate::JobError> {
        loop {
            let mut queue = self.queue.lock().await;

            // Get next job
            let job = match queue.next_job() {
                Some(job) => job,
                None => {
                    drop(queue);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };

            let job_id = job.id.clone();
            let _job_name = job.name.clone();

            // Release queue lock before acquiring semaphore
            drop(queue);

            info!("Processing job: {} (priority: {:?})", job_id, job.priority);

            // Acquire semaphore slot
            let permit = match self.semaphore.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(e) => {
                    warn!("Failed to acquire semaphore slot: {}", e);
                    continue;
                }
            };

            // Spawn job execution
            let scheduler = self.clone();
            let job_clone = job.clone();

            tokio::spawn(async move {
                let result = match timeout(
                    Duration::from_millis(scheduler.config.job_timeout_ms),
                    scheduler.execute_job(job_clone),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => {
                        error!("Job {} timed out", job_id);
                        JobResult {
                            job_id: job_id.clone(),
                            success: false,
                            output: None,
                            error: Some("job timeout".to_string()),
                            completed_at: Utc::now(),
                        }
                    }
                };

                // Update job registry and queue
                let mut queue = scheduler.queue.lock().await;
                let mut registry = scheduler.job_registry.lock().await;

                if let Some(job) = registry.get_mut(&job_id) {
                    job.status = if result.success { JobStatus::Completed } else { JobStatus::Failed };
                    job.completed_at = Some(Utc::now());
                    job.result = Some(result.clone());

                    if !result.success {
                        // Attempt retry if configured
                        if job.can_retry() {
                            info!("Retrying job {} (attempt {}/{})", job_id, job.retry_count, job.retry_count + 1);
                            let backoff = match queue.retry_job(&job_id) {
                                Ok(backoff) => backoff,
                                Err(_) => {
                                    let _failed = job.clone().fail("max retries exceeded".to_string());
                                    drop(queue);
                                    drop(registry);
                                    return;
                                }
                            };

                            // Wait for backoff period
                            drop(queue);
                            tokio::time::sleep(backoff).await;

                            // Re-queue the job
                            let mut queue = scheduler.queue.lock().await;
                            if let Some(requeued_job) = registry.get(&job_id) {
                                let reconfig = JobConfig::new(
                                    requeued_job.name.clone(),
                                    requeued_job.payload.clone(),
                                )
                                .with_max_retries(requeued_job.max_retries)
                                .with_priority(requeued_job.priority.clone());

                                let _ = queue.submit(reconfig);
                            }
                            drop(queue);
                        } else {
                            let _failed = job.clone().fail("max retries exceeded".to_string());
                        }
                    }
                }

                drop(permit);
            });
        }
    }

    async fn execute_job(&self, job: Job) -> JobResult {
        let job_id = job.id.clone();

        // Find handler
        let handler = match self.handlers.get(&job.name) {
            Some(h) => h.clone(),
            None => {
                return JobResult {
                    job_id: job_id.clone(),
                    success: false,
                    output: None,
                    error: Some(format!("no handler registered for job: {}", job.name)),
                    completed_at: Utc::now(),
                };
            }
        };

        // Execute with handler
        match handler.execute(&job).await {
            Ok(output) => {
                info!("Job {} completed successfully", job_id);
                JobResult {
                    job_id: job_id.clone(),
                    success: true,
                    output: Some(output),
                    error: None,
                    completed_at: Utc::now(),
                }
            }
            Err(e) => {
                warn!("Job {} failed: {}", job_id, e);
                JobResult {
                    job_id: job_id.clone(),
                    success: false,
                    output: None,
                    error: Some(e),
                    completed_at: Utc::now(),
                }
            }
        }
    }

    /// Get scheduler statistics.
    pub async fn stats(&self) -> SchedulerStats {
        let queue = self.queue.lock().await;
        let registry = self.job_registry.lock().await;
        let queue_stats = queue.stats();

        let mut total_retried = 0u64;
        for job in registry.values() {
            total_retried += job.retry_count as u64;
        }

        SchedulerStats {
            total_submitted: queue_stats.total as u64 + registry.len() as u64,
            total_completed: queue_stats.completed as u64 + registry.values().filter(|j| j.status == JobStatus::Completed).count() as u64,
            total_failed: queue_stats.failed as u64 + registry.values().filter(|j| j.status == JobStatus::Failed).count() as u64,
            total_retried,
            active_jobs: queue_stats.running,
            queued_jobs: queue_stats.pending,
        }
    }

    /// Cancel a job by ID.
    pub async fn cancel_job(&self, job_id: &JobId) -> Result<(), crate::JobError> {
        let mut queue = self.queue.lock().await;
        queue.cancel_job(job_id)
    }
}

impl Clone for JobScheduler {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            queue: Mutex::new(JobQueue::new(self.config.max_queue_size)),
            semaphore: self.semaphore.clone(),
            handlers: self.handlers.clone(),
            job_registry: Mutex::new(HashMap::new()),
        }
    }
}
