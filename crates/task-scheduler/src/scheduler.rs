//! Job scheduler with concurrency control, durable stores, and graceful drain.
//!
//! # Stores
//!
//! - [`JobScheduler::new`] uses an in-memory [`crate::MemoryJobStore`] (restart loses the queue).
//! - [`JobScheduler::open_journal`] / [`JobScheduler::from_config`] can attach a
//!   [`crate::JournalJobStore`]. On reopen, `Running` jobs are recovered as `Pending`.
//!
//! # Cancellation
//!
//! Pending jobs are removed from the ready queue. Running jobs are hard-aborted via
//! `AbortHandle`; the handler future is dropped and status stays `Cancelled`.

use async_trait::async_trait;
use chrono::Utc;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::timeout;
use tracing::{error, info, warn};

use crate::job::{Job, JobConfig, JobId, JobStatus};
use crate::queue::{JobQueue, RetryConfig};
use crate::store::{JobEvent, JobStore, JournalJobStore, MemoryJobStore, StoreError};
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
    store: Arc<dyn JobStore>,
    semaphore: Arc<Semaphore>,
    handlers: Arc<HashMap<String, Arc<dyn JobHandler>>>,
    job_registry: Arc<Mutex<HashMap<JobId, Job>>>,
    abort_handles: Arc<std::sync::Mutex<HashMap<JobId, AbortHandle>>>,
    shutdown: Arc<AtomicBool>,
    wake: Arc<Notify>,
    total_submitted: Arc<AtomicU64>,
    total_completed: Arc<AtomicU64>,
    total_failed: Arc<AtomicU64>,
    total_retried: Arc<AtomicU64>,
    active_jobs: Arc<AtomicU64>,
}

impl JobScheduler {
    /// Create a scheduler with an in-memory store.
    pub fn new(config: SchedulerConfig) -> Self {
        Self::with_store(config, Arc::new(MemoryJobStore::new()))
    }

    /// Create a scheduler with an explicit store (memory or journal).
    pub fn with_store(config: SchedulerConfig, store: Arc<dyn JobStore>) -> Self {
        let retry_config = RetryConfig {
            max_retries: config.max_retries,
            backoff_base_ms: config.retry_backoff_base_ms,
            backoff_max_ms: config.retry_backoff_max_ms,
        };
        let queue = JobQueue::new(config.max_queue_size).with_retry_config(retry_config);
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent_jobs.max(1)));
        Self {
            config,
            queue: Arc::new(Mutex::new(queue)),
            store,
            semaphore,
            handlers: Arc::new(HashMap::new()),
            job_registry: Arc::new(Mutex::new(HashMap::new())),
            abort_handles: Arc::new(std::sync::Mutex::new(HashMap::new())),
            shutdown: Arc::new(AtomicBool::new(false)),
            wake: Arc::new(Notify::new()),
            total_submitted: Arc::new(AtomicU64::new(0)),
            total_completed: Arc::new(AtomicU64::new(0)),
            total_failed: Arc::new(AtomicU64::new(0)),
            total_retried: Arc::new(AtomicU64::new(0)),
            active_jobs: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Open a journal-backed scheduler and hydrate pending work.
    pub async fn open_journal(
        config: SchedulerConfig,
        path: impl AsRef<Path>,
    ) -> Result<Self, crate::JobError> {
        let store = JournalJobStore::open(path)
            .await
            .map_err(store_err)?;
        let scheduler = Self::with_store(config, Arc::new(store));
        scheduler.hydrate().await?;
        Ok(scheduler)
    }

    /// Build from config; opens journal when `journal_path` is set.
    pub async fn from_config(config: SchedulerConfig) -> Result<Self, crate::JobError> {
        if let Some(path) = config.journal_path.clone() {
            Self::open_journal(config, path).await
        } else {
            Ok(Self::new(config))
        }
    }

    /// Hydrate registry + ready queue from the store (idempotent for empty stores).
    pub async fn hydrate(&self) -> Result<(), crate::JobError> {
        let jobs = self.store.load_all().await.map_err(store_err)?;
        let mut queue = self.queue.lock().await;
        let mut registry = self.job_registry.lock().await;

        for job in jobs {
            let id = job.id.clone();
            let is_pending = job.status == JobStatus::Pending;
            registry.insert(id.clone(), job.clone());
            self.total_submitted.fetch_add(1, Ordering::Relaxed);
            match job.status {
                JobStatus::Completed => {
                    self.total_completed.fetch_add(1, Ordering::Relaxed);
                }
                JobStatus::Failed => {
                    self.total_failed.fetch_add(1, Ordering::Relaxed);
                }
                JobStatus::Pending | JobStatus::Running | JobStatus::Cancelled => {}
            }
            if is_pending {
                queue.requeue(job).map_err(|e| e)?;
            }
        }
        Ok(())
    }

    /// Register a job handler by name. Call before `run`.
    pub fn register_handler(&mut self, name: String, handler: Arc<dyn JobHandler>) {
        let mut map = (*self.handlers).clone();
        map.insert(name, handler);
        self.handlers = Arc::new(map);
    }

    /// Submit a job to the scheduler.
    pub async fn submit(&self, config: JobConfig) -> Result<JobId, crate::JobError> {
        let mut queue = self.queue.lock().await;
        let job = queue.submit(config)?;
        let id = job.id.clone();
        self.store.put(&job).await.map_err(store_err)?;
        {
            let mut registry = self.job_registry.lock().await;
            registry.insert(id.clone(), job.clone());
        }
        self.total_submitted.fetch_add(1, Ordering::Relaxed);
        info!(
            job_id = %id,
            job_name = %job.name,
            priority = ?job.priority,
            retry_count = job.retry_count,
            "job.submitted"
        );
        self.wake.notify_one();
        Ok(id)
    }

    /// Request the run loop to stop (pending jobs are kept; in-flight drain per timeout).
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.wake.notify_waiters();
        info!("scheduler.shutdown");
    }

    /// Run until shutdown, using `config.drain_timeout_ms` for forced abort.
    pub async fn run(&self) -> Result<(), crate::JobError> {
        let drain = Duration::from_millis(self.config.drain_timeout_ms);
        self.run_with_drain(drain).await
    }

    /// Run until shutdown; after `drain_timeout`, abort remaining in-flight work.
    pub async fn run_with_drain(&self, drain_timeout: Duration) -> Result<(), crate::JobError> {
        let mut in_flight: JoinSet<()> = JoinSet::new();
        let mut drain_deadline: Option<Instant> = None;

        loop {
            while let Some(res) = in_flight.try_join_next() {
                if let Err(e) = res {
                    if !e.is_cancelled() {
                        error!(error = %e, "job task join error");
                    }
                }
            }

            let shutting_down = self.shutdown.load(Ordering::SeqCst);

            if shutting_down {
                if drain_deadline.is_none() {
                    drain_deadline = Some(Instant::now() + drain_timeout);
                }
                if in_flight.is_empty() {
                    break;
                }
                let deadline = drain_deadline.unwrap();
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    self.abort_all_running().await;
                    while let Some(res) = in_flight.join_next().await {
                        if let Err(e) = res {
                            if !e.is_cancelled() {
                                error!(error = %e, "job task join error");
                            }
                        }
                    }
                    return Err(crate::JobError::DrainTimeout);
                }
                tokio::select! {
                    res = in_flight.join_next() => {
                        if let Some(Err(e)) = res {
                            if !e.is_cancelled() {
                                error!(error = %e, "job task join error");
                            }
                        }
                    }
                    _ = tokio::time::sleep(remaining) => {
                        self.abort_all_running().await;
                        while let Some(res) = in_flight.join_next().await {
                            if let Err(e) = res {
                                if !e.is_cancelled() {
                                    error!(error = %e, "job task join error");
                                }
                            }
                        }
                        return Err(crate::JobError::DrainTimeout);
                    }
                }
                continue;
            }

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

            let Some(mut job) = job else {
                drop(permit);
                tokio::select! {
                    _ = self.wake.notified() => {}
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
                continue;
            };

            let job_id = job.id.clone();

            {
                let mut registry = self.job_registry.lock().await;
                if let Some(registered) = registry.get_mut(&job_id) {
                    if registered.status == JobStatus::Cancelled {
                        drop(permit);
                        continue;
                    }
                    registered.start();
                    job = registered.clone();
                } else {
                    job.start();
                }
            }

            if let Err(e) = self.store.update(&job, JobEvent::Started).await {
                warn!(job_id = %job_id, error = %e, "failed to persist job.started");
            }

            self.active_jobs.fetch_add(1, Ordering::Relaxed);
            info!(
                job_id = %job_id,
                job_name = %job.name,
                priority = ?job.priority,
                retry_count = job.retry_count,
                "job.started"
            );

            let scheduler = self.clone();
            let job_clone = job.clone();
            let timeout_ms = job
                .timeout_ms
                .unwrap_or(self.config.job_timeout_ms);

            let job_id_for_map = job_id.clone();
            let abort_handle = in_flight.spawn(async move {
                let permit_guard = PermitGuard {
                    permit: Some(permit),
                    scheduler: scheduler.clone(),
                    job_id: job_id.clone(),
                };

                let result = match timeout(
                    Duration::from_millis(timeout_ms),
                    scheduler.execute_job(&job_clone),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => {
                        error!(job_id = %job_id, "job timeout");
                        JobResult {
                            job_id: job_id.clone(),
                            success: false,
                            output: None,
                            error: Some("job timeout".to_string()),
                            completed_at: Utc::now(),
                        }
                    }
                };

                let retry = scheduler.finish_job(job_id.clone(), result).await;
                // Release concurrency slot before any retry backoff.
                drop(permit_guard);

                if let Some((backoff, requeue_job)) = retry {
                    let scheduler = scheduler.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(backoff).await;
                        scheduler.requeue_after_backoff(requeue_job).await;
                    });
                }
            });

            self.abort_handles
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(job_id_for_map, abort_handle);
        }

        while let Some(res) = in_flight.join_next().await {
            if let Err(e) = res {
                if !e.is_cancelled() {
                    error!(error = %e, "job task join error");
                }
            }
        }

        Ok(())
    }

    async fn abort_all_running(&self) {
        let handles: Vec<_> = {
            let mut map = self
                .abort_handles
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            map.drain().map(|(_, h)| h).collect()
        };
        for handle in handles {
            handle.abort();
        }
        let mut registry = self.job_registry.lock().await;
        for job in registry.values_mut() {
            if job.status == JobStatus::Running {
                job.cancel();
                let _ = self.store.update(job, JobEvent::Cancelled).await;
                info!(
                    job_id = %job.id,
                    job_name = %job.name,
                    priority = ?job.priority,
                    retry_count = job.retry_count,
                    "job.cancelled"
                );
            }
        }
    }

    /// Returns `Some((backoff, job))` when a retry should be scheduled after releasing the permit.
    async fn finish_job(&self, job_id: JobId, result: JobResult) -> Option<(Duration, Job)> {
        self.abort_handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&job_id);

        let mut registry = self.job_registry.lock().await;
        let Some(job) = registry.get_mut(&job_id) else {
            warn!(job_id = %job_id, "finished unknown job");
            return None;
        };

        if job.status == JobStatus::Cancelled {
            return None;
        }

        if result.success {
            job.complete(result);
            let snapshot = job.clone();
            drop(registry);
            let _ = self.store.update(&snapshot, JobEvent::Completed).await;
            self.total_completed.fetch_add(1, Ordering::Relaxed);
            info!(
                job_id = %snapshot.id,
                job_name = %snapshot.name,
                priority = ?snapshot.priority,
                retry_count = snapshot.retry_count,
                "job.completed"
            );
            return None;
        }

        let err = result
            .error
            .clone()
            .unwrap_or_else(|| "job failed".to_string());
        job.fail(err);
        job.result = Some(result);

        if !job.can_retry() {
            let snapshot = job.clone();
            drop(registry);
            let _ = self.store.update(&snapshot, JobEvent::Failed).await;
            self.total_failed.fetch_add(1, Ordering::Relaxed);
            info!(
                job_id = %snapshot.id,
                job_name = %snapshot.name,
                priority = ?snapshot.priority,
                retry_count = snapshot.retry_count,
                "job.failed"
            );
            return None;
        }

        let retry_count_after = job.retry_count + 1;
        let max_retries = job.max_retries;
        job.prepare_retry();
        self.total_retried.fetch_add(1, Ordering::Relaxed);
        let requeue_job = job.clone();
        drop(registry);

        let _ = self
            .store
            .update(&requeue_job, JobEvent::Retried)
            .await;
        let backoff = {
            let queue = self.queue.lock().await;
            queue.retry_config().calculate_backoff(retry_count_after)
        };

        info!(
            job_id = %job_id,
            job_name = %requeue_job.name,
            priority = ?requeue_job.priority,
            retry_count = requeue_job.retry_count,
            attempt = retry_count_after,
            max_retries,
            "job.retried"
        );

        Some((backoff, requeue_job))
    }

    async fn requeue_after_backoff(&self, requeue_job: Job) {
        let job_id = requeue_job.id.clone();
        let mut queue = self.queue.lock().await;
        let mut registry = self.job_registry.lock().await;
        let Some(job) = registry.get_mut(&job_id) else {
            return;
        };
        if job.status == JobStatus::Cancelled {
            return;
        }
        *job = requeue_job.clone();
        if let Err(e) = queue.requeue(requeue_job) {
            warn!(job_id = %job_id, error = %e, "failed to requeue job");
            job.fail(format!("requeue failed: {e}"));
            let snapshot = job.clone();
            drop(registry);
            drop(queue);
            let _ = self.store.update(&snapshot, JobEvent::Failed).await;
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
            Ok(output) => JobResult {
                job_id: job_id.clone(),
                success: true,
                output: Some(output),
                error: None,
                completed_at: Utc::now(),
            },
            Err(e) => JobResult {
                job_id: job_id.clone(),
                success: false,
                output: None,
                error: Some(e),
                completed_at: Utc::now(),
            },
        }
    }

    /// Get a snapshot of a job from the registry (falls back to store).
    pub async fn get_job(&self, job_id: &JobId) -> Option<Job> {
        {
            let registry = self.job_registry.lock().await;
            if let Some(job) = registry.get(job_id) {
                return Some(job.clone());
            }
        }
        self.store.get(job_id).await.ok().flatten()
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

    /// Cancel a job by ID. Running jobs are hard-aborted.
    pub async fn cancel_job(&self, job_id: &JobId) -> Result<(), crate::JobError> {
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
                let snapshot = job.clone();
                drop(registry);
                drop(queue);
                self.store
                    .update(&snapshot, JobEvent::Cancelled)
                    .await
                    .map_err(store_err)?;
                info!(
                    job_id = %snapshot.id,
                    job_name = %snapshot.name,
                    priority = ?snapshot.priority,
                    retry_count = snapshot.retry_count,
                    "job.cancelled"
                );
                Ok(())
            }
            JobStatus::Running => {
                job.cancel();
                let snapshot = job.clone();
                drop(registry);
                drop(queue);
                if let Some(handle) = self
                    .abort_handles
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(job_id)
                {
                    handle.abort();
                }
                self.store
                    .update(&snapshot, JobEvent::Cancelled)
                    .await
                    .map_err(store_err)?;
                info!(
                    job_id = %snapshot.id,
                    job_name = %snapshot.name,
                    priority = ?snapshot.priority,
                    retry_count = snapshot.retry_count,
                    "job.cancelled"
                );
                Ok(())
            }
        }
    }
}

struct PermitGuard {
    permit: Option<OwnedSemaphorePermit>,
    scheduler: JobScheduler,
    job_id: JobId,
}

impl Drop for PermitGuard {
    fn drop(&mut self) {
        self.permit.take();
        self.scheduler
            .active_jobs
            .fetch_sub(1, Ordering::Relaxed);
        self.scheduler
            .abort_handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.job_id);
        self.scheduler.wake.notify_one();
    }
}

fn store_err(e: StoreError) -> crate::JobError {
    crate::JobError::Store(e.to_string())
}

impl Clone for JobScheduler {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            queue: Arc::clone(&self.queue),
            store: Arc::clone(&self.store),
            semaphore: Arc::clone(&self.semaphore),
            handlers: Arc::clone(&self.handlers),
            job_registry: Arc::clone(&self.job_registry),
            abort_handles: Arc::clone(&self.abort_handles),
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
