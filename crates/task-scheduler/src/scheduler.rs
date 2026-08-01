//! Job scheduler with concurrency control and retry logic.

use async_trait::async_trait;
use chrono::Utc;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify, Semaphore};
use tokio::time::timeout;
use tracing::{error, info, warn};

use crate::job::{Job, JobConfig, JobId, JobStatus};
use crate::queue::{JobQueue, RetryConfig};
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
    queue: Arc<Mutex<JobQueue>>,
    semaphore: Arc<Semaphore>,
    handlers: Arc<HashMap<String, Arc<dyn JobHandler>>>,
    job_registry: Arc<Mutex<HashMap<JobId, Job>>>,
    shutdown: Arc<AtomicBool>,
    wake: Arc<Notify>,
    total_submitted: Arc<AtomicU64>,
    total_completed: Arc<AtomicU64>,
    total_failed: Arc<AtomicU64>,
    total_retried: Arc<AtomicU64>,
    active_jobs: Arc<AtomicU64>,
}

impl JobScheduler {
    pub fn new(config: SchedulerConfig) -> Self {
        let retry_config = RetryConfig {
            max_retries: config.max_retries,
            backoff_base_ms: config.retry_backoff_base_ms,
            backoff_max_ms: config.retry_backoff_max_ms,
        };
        let queue = JobQueue::new(config.max_queue_size).with_retry_config(retry_config);
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent_jobs));
        Self {
            config,
            queue: Arc::new(Mutex::new(queue)),
            semaphore,
            handlers: Arc::new(HashMap::new()),
            job_registry: Arc::new(Mutex::new(HashMap::new())),
            shutdown: Arc::new(AtomicBool::new(false)),
            wake: Arc::new(Notify::new()),
            total_submitted: Arc::new(AtomicU64::new(0)),
            total_completed: Arc::new(AtomicU64::new(0)),
            total_failed: Arc::new(AtomicU64::new(0)),
            total_retried: Arc::new(AtomicU64::new(0)),
            active_jobs: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Register a job handler by name. Call before `run`.
    pub fn register_handler(&mut self, name: String, handler: Arc<dyn JobHandler>) {
        let mut map = (*self.handlers).clone();
        map.insert(name, handler);
        self.handlers = Arc::new(map);
    }

    /// Submit a job to the scheduler.
    pub async fn submit(&self, config: JobConfig) -> Result<JobId, crate::JobError> {
        // Lock order: queue then registry
        let mut queue = self.queue.lock().await;
        let job = queue.submit(config)?;
        let id = job.id.clone();
        {
            let mut registry = self.job_registry.lock().await;
            registry.insert(id.clone(), job);
        }
        self.total_submitted.fetch_add(1, Ordering::Relaxed);
        self.wake.notify_one();
        Ok(id)
    }

    /// Request the run loop to stop after in-flight work drains.
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.wake.notify_waiters();
    }

    /// Start processing jobs from the queue until shutdown is requested.
    pub async fn run(&self) -> Result<(), crate::JobError> {
        let mut in_flight: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

        loop {
            // Reap finished tasks
            while let Some(res) = in_flight.try_join_next() {
                if let Err(e) = res {
                    error!("job task join error: {e}");
                }
            }

            if self.shutdown.load(Ordering::SeqCst) && in_flight.is_empty() {
                let pending = self.queue.lock().await.pending_count();
                if pending == 0 {
                    break;
                }
                // Shutting down with pending work: stop accepting new starts.
                break;
            }

            if self.shutdown.load(Ordering::SeqCst) {
                // Wait for in-flight to finish; do not start new jobs.
                if let Some(res) = in_flight.join_next().await {
                    if let Err(e) = res {
                        error!("job task join error: {e}");
                    }
                }
                continue;
            }

            // Acquire concurrency slot before dequeuing so jobs stay cancelable in-queue.
            let permit = match self.semaphore.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    tokio::select! {
                        _ = self.wake.notified() => {}
                        _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                    }
                    continue;
                }
            };

            let job = {
                let mut queue = self.queue.lock().await;
                queue.next_job()
            };

            let Some(job) = job else {
                drop(permit);
                if self.shutdown.load(Ordering::SeqCst) {
                    continue;
                }
                tokio::select! {
                    _ = self.wake.notified() => {}
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
                continue;
            };

            let job_id = job.id.clone();

            // Mirror Running into the registry
            {
                let mut registry = self.job_registry.lock().await;
                if let Some(registered) = registry.get_mut(&job_id) {
                    if registered.status == JobStatus::Cancelled {
                        // Cancelled while queued; skip execution
                        drop(permit);
                        continue;
                    }
                    registered.start();
                }
            }

            self.active_jobs.fetch_add(1, Ordering::Relaxed);
            info!("Processing job: {} (priority: {:?})", job_id, job.priority);

            let scheduler = self.clone();
            let job_clone = job.clone();

            in_flight.spawn(async move {
                let result = match timeout(
                    Duration::from_millis(scheduler.config.job_timeout_ms),
                    scheduler.execute_job(&job_clone),
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

                scheduler.finish_job(job_id, result).await;
                scheduler.active_jobs.fetch_sub(1, Ordering::Relaxed);
                drop(permit);
                scheduler.wake.notify_one();
            });
        }

        while let Some(res) = in_flight.join_next().await {
            if let Err(e) = res {
                error!("job task join error: {e}");
            }
        }

        Ok(())
    }

    async fn finish_job(&self, job_id: JobId, result: JobResult) {
        // Phase 1: update registry (no queue lock held)
        let retry_plan = {
            let mut registry = self.job_registry.lock().await;
            let Some(job) = registry.get_mut(&job_id) else {
                warn!("finished unknown job {}", job_id);
                return;
            };

            // Honour cancel that landed while running
            if job.status == JobStatus::Cancelled {
                return;
            }

            if result.success {
                job.complete(result);
                self.total_completed.fetch_add(1, Ordering::Relaxed);
                return;
            }

            job.fail(
                result
                    .error
                    .clone()
                    .unwrap_or_else(|| "job failed".to_string()),
            );
            job.result = Some(result);

            if !job.can_retry() {
                self.total_failed.fetch_add(1, Ordering::Relaxed);
                return;
            }

            let retry_count_after = job.retry_count + 1;
            let max_retries = job.max_retries;
            job.prepare_retry();
            self.total_retried.fetch_add(1, Ordering::Relaxed);
            Some((retry_count_after, max_retries, job.clone()))
        };

        let Some((retry_count_after, max_retries, requeue_job)) = retry_plan else {
            return;
        };

        let backoff = {
            let queue = self.queue.lock().await;
            queue.retry_config().calculate_backoff(retry_count_after)
        };

        info!(
            "Retrying job {} (attempt {}/{})",
            job_id, retry_count_after, max_retries
        );

        tokio::time::sleep(backoff).await;

        // Phase 2: requeue — lock order queue then registry
        let mut queue = self.queue.lock().await;
        let mut registry = self.job_registry.lock().await;
        let Some(job) = registry.get_mut(&job_id) else {
            return;
        };
        if job.status == JobStatus::Cancelled {
            return;
        }
        if let Err(e) = queue.requeue(requeue_job) {
            warn!("failed to requeue job {}: {e}", job_id);
            job.fail(format!("requeue failed: {e}"));
            self.total_failed.fetch_add(1, Ordering::Relaxed);
        } else {
            drop(registry);
            drop(queue);
            self.wake.notify_one();
        }
    }

    async fn execute_job(&self, job: &Job) -> JobResult {
        let job_id = job.id.clone();

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

        match handler.execute(job).await {
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

    /// Get a snapshot of a job from the registry.
    pub async fn get_job(&self, job_id: &JobId) -> Option<Job> {
        let registry = self.job_registry.lock().await;
        registry.get(job_id).cloned()
    }

    /// Get scheduler statistics.
    pub async fn stats(&self) -> SchedulerStats {
        let queue = self.queue.lock().await;
        let queue_stats = queue.stats();

        SchedulerStats {
            total_submitted: self.total_submitted.load(Ordering::Relaxed),
            total_completed: self.total_completed.load(Ordering::Relaxed),
            total_failed: self.total_failed.load(Ordering::Relaxed),
            total_retried: self.total_retried.load(Ordering::Relaxed),
            active_jobs: self.active_jobs.load(Ordering::Relaxed) as usize,
            queued_jobs: queue_stats.pending,
        }
    }

    /// Cancel a job by ID.
    pub async fn cancel_job(&self, job_id: &JobId) -> Result<(), crate::JobError> {
        // Lock order: queue then registry
        let mut queue = self.queue.lock().await;
        let mut registry = self.job_registry.lock().await;
        let Some(job) = registry.get_mut(job_id) else {
            return Err(crate::JobError::NotFound(job_id.clone()));
        };

        match job.status {
            JobStatus::Completed | JobStatus::Failed | JobStatus::Cancelled => {
                Err(crate::JobError::Execution(format!(
                    "job {} already finished with status {}",
                    job_id, job.status
                )))
            }
            JobStatus::Pending => {
                let _ = queue.cancel_job(job_id);
                job.cancel();
                Ok(())
            }
            JobStatus::Running => {
                job.cancel();
                Ok(())
            }
        }
    }
}

impl Clone for JobScheduler {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            queue: Arc::clone(&self.queue),
            semaphore: Arc::clone(&self.semaphore),
            handlers: Arc::clone(&self.handlers),
            job_registry: Arc::clone(&self.job_registry),
            shutdown: Arc::clone(&self.shutdown),
            wake: Arc::clone(&self.wake),
            total_submitted: Arc::clone(&self.total_submitted),
            total_completed: Arc::clone(&self.total_completed),
            total_failed: Arc::clone(&self.total_failed),
            total_retried: Arc::clone(&self.total_retried),
            active_jobs: Arc::clone(&self.active_jobs),
        }
    }
}
