use async_trait::async_trait;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use task_scheduler::{
    Job, JobConfig, JobError, JobEvent, JobHandler, JobId, JobPriority, JobQueue, JobResult,
    JobScheduler, JobStatus, JobStore, JournalJobStore, MemoryJobStore, RetryConfig,
    SchedulerConfig, StoreError,
};
use tokio::sync::Mutex;
use types::{Capability, CapabilitySet};

fn archived_journal(path: &std::path::Path) -> std::path::PathBuf {
    let prefix = format!("{}.archive-", path.file_name().unwrap().to_string_lossy());
    std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|candidate| {
            candidate
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(&prefix)
        })
        .expect("damaged journal must be archived")
}

// ===== Job tests =====

#[test]
fn job_new_has_pending_status() {
    let job = Job::new(
        "test".to_string(),
        serde_json::json!({}),
        JobPriority::Normal,
    );
    assert_eq!(job.status, JobStatus::Pending);
    assert_eq!(job.retry_count, 0);
    assert!(job.started_at.is_none());
    assert!(job.completed_at.is_none());
}

#[test]
fn job_id_produces_unique_ids() {
    let id1 = JobId::new();
    let id2 = JobId::new();
    assert_ne!(id1, id2);
}

#[test]
fn job_start_sets_running() {
    let mut job = Job::new("test".to_string(), serde_json::json!({}), JobPriority::High);
    job.start();
    assert_eq!(job.status, JobStatus::Running);
    assert!(job.started_at.is_some());
}

#[test]
fn job_complete_sets_completed() {
    let mut job = Job::new(
        "test".to_string(),
        serde_json::json!({}),
        JobPriority::Normal,
    );
    let result = JobResult {
        job_id: job.id.clone(),
        success: true,
        output: Some(serde_json::json!({"ok": true})),
        error: None,
        completed_at: chrono::Utc::now(),
    };
    job.complete(result);
    assert_eq!(job.status, JobStatus::Completed);
    assert!(job.result.is_some());
}

#[test]
fn job_fail_sets_failed() {
    let mut job = Job::new("test".to_string(), serde_json::json!({}), JobPriority::Low);
    job.fail("something broke".to_string());
    assert_eq!(job.status, JobStatus::Failed);
    assert_eq!(job.error, Some("something broke".to_string()));
}

#[test]
fn job_cancel_sets_cancelled() {
    let mut job = Job::new(
        "test".to_string(),
        serde_json::json!({}),
        JobPriority::Critical,
    );
    job.cancel();
    assert_eq!(job.status, JobStatus::Cancelled);
}

#[test]
fn job_can_retry_when_failed_and_retries_remaining() {
    let mut job = Job::new(
        "test".to_string(),
        serde_json::json!({}),
        JobPriority::Normal,
    )
    .with_max_retries(3);
    job.fail("error".to_string());
    assert!(job.can_retry());
}

#[test]
fn job_cannot_retry_when_max_retries_exceeded() {
    let mut job = Job::new(
        "test".to_string(),
        serde_json::json!({}),
        JobPriority::Normal,
    )
    .with_max_retries(0);
    job.fail("error".to_string());
    assert!(!job.can_retry());
}

#[test]
fn job_increment_retry() {
    let mut job = Job::new(
        "test".to_string(),
        serde_json::json!({}),
        JobPriority::Normal,
    );
    assert_eq!(job.retry_count, 0);
    job.increment_retry();
    assert_eq!(job.retry_count, 1);
    job.increment_retry();
    assert_eq!(job.retry_count, 2);
}

#[test]
fn job_prepare_retry_resets_to_pending() {
    let mut job = Job::new(
        "test".to_string(),
        serde_json::json!({}),
        JobPriority::Normal,
    )
    .with_max_retries(3);
    job.fail("error".to_string());
    job.prepare_retry();
    assert_eq!(job.status, JobStatus::Pending);
    assert_eq!(job.retry_count, 1);
    assert!(job.error.is_none());
}

// ===== JobQueue tests =====

#[test]
fn job_queue_submit_and_next() {
    let mut queue = JobQueue::new(100);
    let job = queue
        .submit(JobConfig::new("test".to_string(), serde_json::json!({})))
        .unwrap();
    let id = job.id.clone();

    assert_eq!(queue.len(), 1);
    let next = queue.next_job().unwrap();
    assert_eq!(next.id, id);
    assert_eq!(next.status, JobStatus::Running);
    assert_eq!(queue.len(), 0);
}

#[test]
fn job_queue_empty_returns_none() {
    let mut queue = JobQueue::new(100);
    assert!(queue.next_job().is_none());
    assert!(queue.is_empty());
}

#[test]
fn job_queue_queue_full_error() {
    let mut queue = JobQueue::new(2);
    queue
        .submit(JobConfig::new("a".to_string(), serde_json::json!({})))
        .unwrap();
    queue
        .submit(JobConfig::new("b".to_string(), serde_json::json!({})))
        .unwrap();

    let result = queue.submit(JobConfig::new("c".to_string(), serde_json::json!({})));
    assert_eq!(result.unwrap_err(), JobError::QueueFull);
    assert_eq!(queue.len(), 2);
}

#[test]
fn job_queue_priority_ordering() {
    let mut queue = JobQueue::new(100);
    queue
        .submit(JobConfig::new("low".to_string(), serde_json::json!({})))
        .unwrap();
    queue
        .submit(
            JobConfig::new("high".to_string(), serde_json::json!({}))
                .with_priority(JobPriority::High),
        )
        .unwrap();
    queue
        .submit(
            JobConfig::new("critical".to_string(), serde_json::json!({}))
                .with_priority(JobPriority::Critical),
        )
        .unwrap();

    let job = queue.next_job().unwrap();
    assert_eq!(job.priority, JobPriority::Critical);
    assert_eq!(job.name, "critical");
}

#[test]
fn job_queue_fifo_within_same_priority() {
    let mut queue = JobQueue::new(100);
    let first = queue
        .submit(JobConfig::new("a".to_string(), serde_json::json!({})))
        .unwrap();
    let _second = queue
        .submit(JobConfig::new("b".to_string(), serde_json::json!({})))
        .unwrap();

    let job = queue.next_job().unwrap();
    assert_eq!(job.id, first.id);
    assert_eq!(job.name, "a");
}

#[test]
fn job_queue_complete_job() {
    let mut queue = JobQueue::new(100);
    let job = queue
        .submit(JobConfig::new("test".to_string(), serde_json::json!({})))
        .unwrap();
    let id = job.id.clone();

    let result = JobResult {
        job_id: id.clone(),
        success: true,
        output: None,
        error: None,
        completed_at: chrono::Utc::now(),
    };
    queue.complete_job(&id, result).unwrap();

    assert_eq!(queue.len(), 1);
    let stats = queue.stats();
    assert_eq!(stats.completed, 1);
    assert_eq!(stats.pending, 0);
}

#[test]
fn job_queue_retry_job() {
    let mut queue = JobQueue::new(100);
    let job = queue
        .submit(JobConfig::new("test".to_string(), serde_json::json!({})).with_max_retries(3))
        .unwrap();
    let id = job.id.clone();

    // Pending jobs are retryable; prepare_retry bumps count and keeps them queued
    let backoff = queue.retry_job(&id).unwrap();
    assert!(backoff.as_millis() > 0);
    assert_eq!(queue.stats().pending, 1);

    let job = queue.next_job().unwrap();
    assert_eq!(job.id, id);
    assert_eq!(job.retry_count, 1);
}

#[test]
fn job_queue_cancel_removes_pending() {
    let mut queue = JobQueue::new(100);
    let job = queue
        .submit(JobConfig::new("test".to_string(), serde_json::json!({})))
        .unwrap();
    let cancelled = queue.cancel_job(&job.id).unwrap();
    assert_eq!(cancelled.status, JobStatus::Cancelled);
    assert!(queue.is_empty());
}

#[test]
fn job_queue_stats() {
    let mut queue = JobQueue::new(100);
    queue
        .submit(JobConfig::new("a".to_string(), serde_json::json!({})))
        .unwrap();
    queue
        .submit(JobConfig::new("b".to_string(), serde_json::json!({})))
        .unwrap();

    let stats = queue.stats();
    assert_eq!(stats.total, 2);
    assert_eq!(stats.pending, 2);
    assert_eq!(stats.running, 0);
    assert_eq!(stats.completed, 0);
    assert_eq!(stats.failed, 0);
}

// ===== RetryConfig tests =====

#[test]
fn retry_config_backoff_increases() {
    let config = RetryConfig::default();

    let backoff1 = config.calculate_backoff(0);
    let backoff2 = config.calculate_backoff(1);
    let backoff3 = config.calculate_backoff(2);

    // Allow jitter: base exponential still trends up across samples
    assert!(backoff2.as_millis() >= backoff1.as_millis() / 2);
    assert!(backoff3.as_millis() >= backoff2.as_millis() / 2);
    assert!(backoff3.as_millis() > backoff1.as_millis());
}

#[test]
fn retry_config_backoff_capped() {
    let config = RetryConfig {
        max_retries: 10,
        backoff_base_ms: 1000,
        backoff_max_ms: 5000,
    };

    let backoff = config.calculate_backoff(10);
    // Cap + up to 20% jitter
    assert!(backoff.as_millis() <= 6000);
}

// ===== JobScheduler tests =====

struct OkHandler;

struct RejectPutStore;

struct AppendThenErrorStore(MemoryJobStore);

#[async_trait]
impl JobStore for AppendThenErrorStore {
    async fn put(&self, job: &Job) -> Result<(), StoreError> {
        self.0.put(job).await?;
        Err(std::io::Error::other("ack lost after append").into())
    }
    async fn get(&self, id: &JobId) -> Result<Option<Job>, StoreError> {
        self.0.get(id).await
    }
    async fn update(&self, job: &Job, event: JobEvent) -> Result<(), StoreError> {
        self.0.update(job, event).await
    }
    async fn pending(&self) -> Result<Vec<Job>, StoreError> {
        self.0.pending().await
    }
    async fn load_all(&self) -> Result<Vec<Job>, StoreError> {
        self.0.load_all().await
    }
}

struct RejectEventStore {
    inner: MemoryJobStore,
    denied: JobEvent,
}

#[async_trait]
impl JobStore for RejectEventStore {
    async fn put(&self, job: &Job) -> Result<(), StoreError> {
        self.inner.put(job).await
    }
    async fn get(&self, id: &JobId) -> Result<Option<Job>, StoreError> {
        self.inner.get(id).await
    }
    async fn update(&self, job: &Job, event: JobEvent) -> Result<(), StoreError> {
        if event == self.denied {
            return Err(std::io::Error::other("injected transition failure").into());
        }
        self.inner.update(job, event).await
    }
    async fn pending(&self) -> Result<Vec<Job>, StoreError> {
        self.inner.pending().await
    }
    async fn load_all(&self) -> Result<Vec<Job>, StoreError> {
        self.inner.load_all().await
    }
}

#[test]
fn failed_start_never_invokes_handler_and_preserves_pending_job() {
    runtime().block_on(async {
        let mut scheduler = JobScheduler::with_store(
            SchedulerConfig::default(),
            Arc::new(RejectEventStore {
                inner: MemoryJobStore::new(),
                denied: JobEvent::Started,
            }),
        );
        let calls = Arc::new(AtomicU32::new(0));
        struct CountingHandler(Arc<AtomicU32>);
        #[async_trait]
        impl JobHandler for CountingHandler {
            async fn execute(&self, _job: &Job) -> Result<serde_json::Value, String> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(serde_json::json!({}))
            }
        }
        scheduler.register_handler("test".into(), Arc::new(CountingHandler(calls.clone())));
        let id = scheduler
            .submit(JobConfig::new("test".into(), serde_json::json!({})))
            .await
            .unwrap();
        assert!(scheduler.run().await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            scheduler.get_job(&id).await.unwrap().status,
            JobStatus::Pending
        );
    });
}

#[test]
fn failed_completion_persistence_is_visible_as_uncertain_and_never_replayed() {
    runtime().block_on(async {
        let store = Arc::new(RejectEventStore {
            inner: MemoryJobStore::new(),
            denied: JobEvent::Completed,
        });
        let mut scheduler = JobScheduler::with_store(SchedulerConfig::default(), store.clone());
        scheduler.register_handler("test".into(), Arc::new(OkHandler));
        let id = scheduler
            .submit(JobConfig::new("test".into(), serde_json::json!({})))
            .await
            .unwrap();
        let scheduler = Arc::new(scheduler);
        let runner = tokio::spawn({
            let scheduler = scheduler.clone();
            async move { scheduler.run().await }
        });
        let job = wait_status(&scheduler, &id, JobStatus::ReconciliationRequired, 100).await;
        assert_eq!(job.status, JobStatus::ReconciliationRequired);
        assert_eq!(scheduler.stats().await.total_completed, 0);
        scheduler.request_shutdown();
        runner.await.unwrap().unwrap();
    });
}

#[async_trait]
impl JobStore for RejectPutStore {
    async fn put(&self, _job: &Job) -> Result<(), StoreError> {
        Err(std::io::Error::other("injected admission failure").into())
    }

    async fn get(&self, _id: &JobId) -> Result<Option<Job>, StoreError> {
        Ok(None)
    }

    async fn update(&self, _job: &Job, _event: JobEvent) -> Result<(), StoreError> {
        Ok(())
    }

    async fn pending(&self) -> Result<Vec<Job>, StoreError> {
        Ok(Vec::new())
    }

    async fn load_all(&self) -> Result<Vec<Job>, StoreError> {
        Ok(Vec::new())
    }
}

#[test]
fn failed_submission_leaves_no_runnable_job() {
    runtime().block_on(async {
        let mut scheduler =
            JobScheduler::with_store(SchedulerConfig::default(), Arc::new(RejectPutStore));
        let calls = Arc::new(AtomicU32::new(0));
        struct CountingHandler(Arc<AtomicU32>);
        #[async_trait]
        impl JobHandler for CountingHandler {
            async fn execute(&self, _job: &Job) -> Result<serde_json::Value, String> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(serde_json::json!({}))
            }
        }
        scheduler.register_handler("test".into(), Arc::new(CountingHandler(calls.clone())));
        let error = scheduler
            .submit(JobConfig::new("test".into(), serde_json::json!({})))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("job_"), "{error}");
        assert_eq!(scheduler.stats().await.queued_jobs, 0);
        let scheduler = Arc::new(scheduler);
        let runner = tokio::spawn({
            let scheduler = scheduler.clone();
            async move { scheduler.run().await }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        scheduler.request_shutdown();
        runner.await.unwrap().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn admission_error_after_append_exposes_reconciliation_id_without_running() {
    runtime().block_on(async {
        let store = Arc::new(AppendThenErrorStore(MemoryJobStore::new()));
        let scheduler = JobScheduler::with_store(SchedulerConfig::default(), store.clone());
        let error = scheduler
            .submit(JobConfig::new("test".into(), serde_json::json!({})))
            .await
            .unwrap_err();
        let JobError::AdmissionUncertain { job_id, .. } = error else {
            panic!("expected typed uncertain admission error");
        };
        assert_eq!(
            store.get(&job_id).await.unwrap().unwrap().status,
            JobStatus::Pending
        );
        assert_eq!(scheduler.stats().await.queued_jobs, 0);
    });
}

#[async_trait]
impl JobHandler for OkHandler {
    async fn execute(&self, _job: &Job) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({"ok": true}))
    }
}

struct FailNTimes {
    failures_remaining: AtomicU32,
}

#[async_trait]
impl JobHandler for FailNTimes {
    fn safe_to_retry(&self) -> bool {
        true
    }

    async fn execute(&self, _job: &Job) -> Result<serde_json::Value, String> {
        let left = self.failures_remaining.load(Ordering::SeqCst);
        if left > 0 {
            self.failures_remaining.fetch_sub(1, Ordering::SeqCst);
            Err(format!("fail-{left}"))
        } else {
            Ok(serde_json::json!({"recovered": true}))
        }
    }
}

#[test]
fn handler_without_replay_guarantee_is_never_retried() {
    runtime().block_on(async {
        struct UnsafeFail(Arc<AtomicU32>);
        #[async_trait]
        impl JobHandler for UnsafeFail {
            async fn execute(&self, _job: &Job) -> Result<serde_json::Value, String> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err("effect may have occurred".into())
            }
        }
        let calls = Arc::new(AtomicU32::new(0));
        let mut scheduler = JobScheduler::new(SchedulerConfig::default().with_backoff_range(1, 1));
        scheduler.register_handler("unsafe".into(), Arc::new(UnsafeFail(calls.clone())));
        let id = scheduler
            .submit(JobConfig::new("unsafe".into(), serde_json::json!({})))
            .await
            .unwrap();
        let scheduler = Arc::new(scheduler);
        let runner = tokio::spawn({
            let scheduler = scheduler.clone();
            async move { scheduler.run().await }
        });
        let job = wait_status(&scheduler, &id, JobStatus::Failed, 100).await;
        assert_eq!(job.retry_count, 0);
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        scheduler.request_shutdown();
        runner.await.unwrap().unwrap();
    });
}

struct SlowHandler {
    delay: Duration,
    started: Arc<Mutex<u32>>,
}

#[async_trait]
impl JobHandler for SlowHandler {
    async fn execute(&self, _job: &Job) -> Result<serde_json::Value, String> {
        {
            let mut n = self.started.lock().await;
            *n += 1;
        }
        tokio::time::sleep(self.delay).await;
        Ok(serde_json::json!({"done": true}))
    }
}

struct HangHandler;

#[async_trait]
impl JobHandler for HangHandler {
    async fn execute(&self, _job: &Job) -> Result<serde_json::Value, String> {
        tokio::time::sleep(Duration::from_secs(60)).await;
        Ok(serde_json::json!({}))
    }
}

struct RetrySafeHangHandler(Arc<AtomicU32>);

#[async_trait]
impl JobHandler for RetrySafeHangHandler {
    fn safe_to_retry(&self) -> bool {
        true
    }

    async fn execute(&self, _job: &Job) -> Result<serde_json::Value, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(60)).await;
        Ok(serde_json::json!({}))
    }
}

struct NetworkHandler;

#[async_trait]
impl JobHandler for NetworkHandler {
    fn required_capabilities(&self) -> &'static [Capability] {
        &[Capability::NetworkEgress]
    }

    async fn execute(&self, _job: &Job) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({"ok": true}))
    }
}

struct LargeOutputHandler;

#[async_trait]
impl JobHandler for LargeOutputHandler {
    async fn execute(&self, _job: &Job) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({"data": "x".repeat(128)}))
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn scheduler_new_with_config() {
    let scheduler = JobScheduler::new(SchedulerConfig::default());
    let rt = runtime();
    let stats = rt.block_on(async { scheduler.stats().await });
    assert_eq!(stats.total_submitted, 0);
    assert_eq!(stats.queued_jobs, 0);
}

#[test]
fn scheduler_submit_and_stats() {
    let scheduler = JobScheduler::new(SchedulerConfig::default());
    let rt = runtime();

    rt.block_on(async {
        let id = scheduler
            .submit(JobConfig::new("test".to_string(), serde_json::json!({})))
            .await
            .unwrap();

        let stats = scheduler.stats().await;
        assert_eq!(stats.queued_jobs, 1);
        assert_eq!(stats.total_submitted, 1);
        assert!(scheduler.get_job(&id).await.is_some());
    });
}

#[test]
fn scheduler_cancel_pending_job() {
    let scheduler = JobScheduler::new(SchedulerConfig::default());
    let rt = runtime();

    rt.block_on(async {
        let id = scheduler
            .submit(JobConfig::new("test".to_string(), serde_json::json!({})))
            .await
            .unwrap();

        scheduler.cancel_job(&id).await.unwrap();

        let stats = scheduler.stats().await;
        assert_eq!(stats.queued_jobs, 0);

        let job = scheduler.get_job(&id).await.unwrap();
        assert_eq!(job.status, JobStatus::Cancelled);
    });
}

#[test]
fn scheduler_runs_handler_to_completion() {
    let mut scheduler = JobScheduler::new(
        SchedulerConfig::default()
            .with_job_timeout(5_000)
            .with_backoff_range(1, 5),
    );
    scheduler.register_handler("test".to_string(), Arc::new(OkHandler));
    let scheduler = Arc::new(scheduler);
    let rt = runtime();

    rt.block_on(async {
        let id = scheduler
            .submit(JobConfig::new("test".to_string(), serde_json::json!({})))
            .await
            .unwrap();

        let runner = {
            let s = Arc::clone(&scheduler);
            tokio::spawn(async move { s.run().await })
        };

        // Wait for completion
        for _ in 0..50 {
            if let Some(job) = scheduler.get_job(&id).await {
                if job.status == JobStatus::Completed {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let job = scheduler.get_job(&id).await.unwrap();
        assert_eq!(job.status, JobStatus::Completed);
        assert!(job.result.as_ref().unwrap().success);

        let stats = scheduler.stats().await;
        assert_eq!(stats.total_completed, 1);
        assert_eq!(stats.total_submitted, 1);

        scheduler.request_shutdown();
        runner.await.unwrap().unwrap();
    });
}

#[test]
fn scheduler_retries_then_succeeds() {
    let mut scheduler = JobScheduler::new(
        SchedulerConfig::default()
            .with_job_timeout(5_000)
            .with_backoff_range(1, 5)
            .with_max_retries(3),
    );
    scheduler.register_handler(
        "flaky".to_string(),
        Arc::new(FailNTimes {
            failures_remaining: AtomicU32::new(2),
        }),
    );
    let scheduler = Arc::new(scheduler);
    let rt = runtime();

    rt.block_on(async {
        let id = scheduler
            .submit(JobConfig::new("flaky".to_string(), serde_json::json!({})).with_max_retries(3))
            .await
            .unwrap();

        let runner = {
            let s = Arc::clone(&scheduler);
            tokio::spawn(async move { s.run().await })
        };

        for _ in 0..100 {
            if let Some(job) = scheduler.get_job(&id).await {
                if job.status == JobStatus::Completed {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let job = scheduler.get_job(&id).await.unwrap();
        assert_eq!(job.status, JobStatus::Completed);
        assert_eq!(job.retry_count, 2);

        let stats = scheduler.stats().await;
        assert_eq!(stats.total_retried, 2);
        assert_eq!(stats.total_completed, 1);

        scheduler.request_shutdown();
        runner.await.unwrap().unwrap();
    });
}

#[test]
fn scheduler_respects_max_concurrent() {
    let started = Arc::new(Mutex::new(0u32));
    let mut scheduler = JobScheduler::new(
        SchedulerConfig::default()
            .with_max_concurrent(2)
            .with_job_timeout(5_000)
            .with_backoff_range(1, 5),
    );
    scheduler.register_handler(
        "slow".to_string(),
        Arc::new(SlowHandler {
            delay: Duration::from_millis(150),
            started: Arc::clone(&started),
        }),
    );
    let scheduler = Arc::new(scheduler);
    let rt = runtime();

    rt.block_on(async {
        for _ in 0..4 {
            scheduler
                .submit(JobConfig::new("slow".to_string(), serde_json::json!({})))
                .await
                .unwrap();
        }

        let runner = {
            let s = Arc::clone(&scheduler);
            tokio::spawn(async move { s.run().await })
        };

        // While work is in flight, active should never exceed 2
        let mut saw_active = false;
        for _ in 0..40 {
            let stats = scheduler.stats().await;
            assert!(stats.active_jobs <= 2);
            if stats.active_jobs > 0 {
                saw_active = true;
            }
            if stats.total_completed == 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        assert!(saw_active);
        let stats = scheduler.stats().await;
        assert_eq!(stats.total_completed, 4);

        scheduler.request_shutdown();
        runner.await.unwrap().unwrap();
    });
}

#[test]
fn scheduler_times_out_hanging_job() {
    let mut scheduler = JobScheduler::new(
        SchedulerConfig::default()
            .with_job_timeout(50)
            .with_backoff_range(1, 5)
            .with_max_retries(2),
    );
    let calls = Arc::new(AtomicU32::new(0));
    scheduler.register_handler(
        "hang".to_string(),
        Arc::new(RetrySafeHangHandler(calls.clone())),
    );
    let scheduler = Arc::new(scheduler);
    let rt = runtime();

    rt.block_on(async {
        let id = scheduler
            .submit(JobConfig::new("hang".to_string(), serde_json::json!({})).with_max_retries(2))
            .await
            .unwrap();

        let runner = {
            let s = Arc::clone(&scheduler);
            tokio::spawn(async move { s.run().await })
        };

        for _ in 0..50 {
            if let Some(job) = scheduler.get_job(&id).await {
                if job.status == JobStatus::ReconciliationRequired {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let job = scheduler.get_job(&id).await.unwrap();
        assert_eq!(job.status, JobStatus::ReconciliationRequired);
        assert_eq!(job.retry_count, 0);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(job
            .error
            .as_deref()
            .is_some_and(|e| e.contains("timed out")));

        scheduler.request_shutdown();
        runner.await.unwrap().unwrap();
    });
}

#[test]
fn scheduler_fails_without_handler() {
    let scheduler = Arc::new(JobScheduler::new(
        SchedulerConfig::default()
            .with_job_timeout(1_000)
            .with_backoff_range(1, 5)
            .with_max_retries(0),
    ));
    let rt = runtime();

    rt.block_on(async {
        let id = scheduler
            .submit(
                JobConfig::new("missing".to_string(), serde_json::json!({})).with_max_retries(0),
            )
            .await
            .unwrap();

        let runner = {
            let s = Arc::clone(&scheduler);
            tokio::spawn(async move { s.run().await })
        };

        for _ in 0..50 {
            if let Some(job) = scheduler.get_job(&id).await {
                if job.status == JobStatus::Failed {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let job = scheduler.get_job(&id).await.unwrap();
        assert_eq!(job.status, JobStatus::Failed);
        assert!(job
            .error
            .as_deref()
            .is_some_and(|e| e.contains("no handler")));

        scheduler.request_shutdown();
        runner.await.unwrap().unwrap();
    });
}

#[test]
fn scheduler_enforces_handler_capabilities_before_queueing() {
    let mut scheduler = JobScheduler::new(SchedulerConfig::default());
    scheduler.register_handler("network".to_string(), Arc::new(NetworkHandler));
    let rt = runtime();

    rt.block_on(async {
        let denied = scheduler
            .submit_authorized(
                JobConfig::new("network".to_string(), serde_json::json!({})),
                &CapabilitySet::default(),
            )
            .await;
        assert_eq!(
            denied,
            Err(JobError::MissingCapability(Capability::NetworkEgress))
        );
        assert_eq!(scheduler.stats().await.total_submitted, 0);

        scheduler
            .submit_authorized(
                JobConfig::new("network".to_string(), serde_json::json!({})),
                &CapabilitySet::new([Capability::NetworkEgress]),
            )
            .await
            .unwrap();
        assert_eq!(scheduler.stats().await.total_submitted, 1);
    });
}

#[test]
fn scheduler_rejects_handler_output_over_the_serialized_limit() {
    let mut scheduler = JobScheduler::new(
        SchedulerConfig::default()
            .with_max_output_bytes(32)
            .with_max_retries(0),
    );
    scheduler.register_handler("large".to_string(), Arc::new(LargeOutputHandler));
    let scheduler = Arc::new(scheduler);
    let rt = runtime();

    rt.block_on(async {
        let id = scheduler
            .submit(JobConfig::new("large".to_string(), serde_json::json!({})).with_max_retries(0))
            .await
            .unwrap();
        let runner = {
            let scheduler = Arc::clone(&scheduler);
            tokio::spawn(async move { scheduler.run().await })
        };
        let job = wait_status(&scheduler, &id, JobStatus::Failed, 50).await;
        assert!(job
            .result
            .as_ref()
            .is_some_and(|result| result.output.is_none()));
        assert!(job
            .error
            .as_deref()
            .is_some_and(|error| error.contains("output exceeds 32 bytes")));
        scheduler.request_shutdown();
        runner.await.unwrap().unwrap();
    });
}

// ===== SchedulerConfig tests =====

#[test]
fn scheduler_config_default_values() {
    let config = SchedulerConfig::default();
    assert_eq!(config.max_concurrent_jobs, 10);
    assert_eq!(config.max_queue_size, 1000);
    assert_eq!(config.max_retries, 3);
    assert_eq!(config.retry_backoff_base_ms, 1000);
    assert_eq!(config.retry_backoff_max_ms, 60000);
    assert_eq!(config.job_timeout_ms, 300000);
    assert_eq!(config.max_output_bytes, 64 * 1024);
    assert_eq!(config.drain_timeout_ms, 30000);
    assert!(config.journal_path.is_none());
}

#[test]
fn scheduler_config_chain() {
    let config = SchedulerConfig::default()
        .with_max_concurrent(5)
        .with_queue_size(200)
        .with_max_retries(5)
        .with_backoff_range(500, 30000)
        .with_job_timeout(60000)
        .with_max_output_bytes(4096)
        .with_drain_timeout(1000)
        .with_journal_path("/tmp/jobs.jsonl");

    assert_eq!(config.max_concurrent_jobs, 5);
    assert_eq!(config.max_queue_size, 200);
    assert_eq!(config.max_retries, 5);
    assert_eq!(config.retry_backoff_base_ms, 500);
    assert_eq!(config.retry_backoff_max_ms, 30000);
    assert_eq!(config.job_timeout_ms, 60000);
    assert_eq!(config.max_output_bytes, 4096);
    assert_eq!(config.drain_timeout_ms, 1000);
    assert_eq!(
        config.journal_path.as_deref(),
        Some(std::path::Path::new("/tmp/jobs.jsonl"))
    );
}

#[test]
fn job_config_new() {
    let config = JobConfig::new("test".to_string(), serde_json::json!({"key": "val"}));
    assert_eq!(config.name, "test");
    assert_eq!(config.priority, JobPriority::default());
    assert_eq!(config.max_retries, 3);
}

#[test]
fn job_config_chain() {
    let config = JobConfig::new("test".to_string(), serde_json::json!({}))
        .with_priority(JobPriority::Critical)
        .with_max_retries(10)
        .with_timeout(std::time::Duration::from_secs(60));

    assert_eq!(config.priority, JobPriority::Critical);
    assert_eq!(config.max_retries, 10);
    assert_eq!(config.timeout, Some(std::time::Duration::from_secs(60)));
}

// ===== Production: journal / cancel / drain / concurrency =====

async fn wait_status(
    scheduler: &JobScheduler,
    id: &JobId,
    want: JobStatus,
    attempts: usize,
) -> Job {
    for _ in 0..attempts {
        if let Some(job) = scheduler.get_job(id).await {
            if job.status == want {
                return job;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    scheduler
        .get_job(id)
        .await
        .unwrap_or_else(|| panic!("job {id} missing while waiting for {want}"))
}

#[test]
fn journal_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("jobs.jsonl");
    let rt = runtime();

    let id = rt.block_on(async {
        let scheduler =
            JobScheduler::open_journal(SchedulerConfig::default().with_backoff_range(1, 5), &path)
                .await
                .unwrap();
        scheduler
            .submit(JobConfig::new("test".to_string(), serde_json::json!({})))
            .await
            .unwrap()
    });

    // Process "crash": drop first scheduler without running.
    rt.block_on(async {
        let mut scheduler = JobScheduler::open_journal(
            SchedulerConfig::default()
                .with_job_timeout(5_000)
                .with_backoff_range(1, 5)
                .with_drain_timeout(2_000),
            &path,
        )
        .await
        .unwrap();
        scheduler.register_handler("test".to_string(), Arc::new(OkHandler));
        let scheduler = Arc::new(scheduler);

        let pending = scheduler.get_job(&id).await.unwrap();
        assert_eq!(pending.status, JobStatus::Pending);

        let runner = {
            let s = Arc::clone(&scheduler);
            tokio::spawn(async move { s.run().await })
        };

        let job = wait_status(&scheduler, &id, JobStatus::Completed, 100).await;
        assert!(job.result.as_ref().unwrap().success);

        scheduler.request_shutdown();
        runner.await.unwrap().unwrap();
    });
}

#[test]
fn interrupted_running_job_requires_reconciliation_without_replaying_handler() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("jobs.jsonl");
    runtime().block_on(async {
        let store = JournalJobStore::open(&path).await.unwrap();
        let mut job = Job::new("test".into(), serde_json::json!({}), JobPriority::Normal);
        store.put(&job).await.unwrap();
        job.start();
        store.update(&job, JobEvent::Started).await.unwrap();
        drop(store);

        let mut scheduler = JobScheduler::open_journal(SchedulerConfig::default(), &path)
            .await
            .unwrap();
        let calls = Arc::new(AtomicU32::new(0));
        struct CountingHandler(Arc<AtomicU32>);
        #[async_trait]
        impl JobHandler for CountingHandler {
            async fn execute(&self, _job: &Job) -> Result<serde_json::Value, String> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(serde_json::json!({}))
            }
        }
        scheduler.register_handler("test".into(), Arc::new(CountingHandler(calls.clone())));
        assert_eq!(
            serde_json::to_value(&scheduler.get_job(&job.id).await.unwrap().status).unwrap(),
            serde_json::json!("ReconciliationRequired")
        );
        let scheduler = Arc::new(scheduler);
        let runner = tokio::spawn({
            let scheduler = scheduler.clone();
            async move { scheduler.run().await }
        });
        tokio::time::sleep(Duration::from_millis(40)).await;
        scheduler.request_shutdown();
        runner.await.unwrap().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn journal_torn_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("jobs.jsonl");
    let rt = runtime();

    let id = rt.block_on(async {
        let scheduler = JobScheduler::open_journal(SchedulerConfig::default(), &path)
            .await
            .unwrap();
        scheduler
            .submit(JobConfig::new("keep".to_string(), serde_json::json!({})))
            .await
            .unwrap()
    });

    // Append a torn (incomplete) JSON line
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(f, "{{\"schemaVersion\":1,\"sequence\":99,\"recordedAt\":").unwrap();
        f.flush().unwrap();
    }

    rt.block_on(async {
        let store = JournalJobStore::open(&path).await.unwrap();
        assert!(store.recovered_torn_tail());
        let job = store.get(&id).await.unwrap().unwrap();
        assert_eq!(job.status, JobStatus::ReconciliationRequired);
        assert_eq!(job.name, "keep");
    });
}

/// An unreadable line in the middle of the job journal is skipped: the
/// store still opens and every readable job survives.
#[test]
fn journal_unreadable_middle_line_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("jobs.jsonl");
    let rt = runtime();

    let id = rt.block_on(async {
        let scheduler = JobScheduler::open_journal(SchedulerConfig::default(), &path)
            .await
            .unwrap();
        scheduler
            .submit(JobConfig::new("keep".to_string(), serde_json::json!({})))
            .await
            .unwrap()
    });
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            f,
            "{{\"schemaVersion\":1,\"sequence\":7,\"job\":{{\"status\":\"gone\"}}}}"
        )
        .unwrap();
        writeln!(f, "not json").unwrap();
    }

    rt.block_on(async {
        let store = JournalJobStore::open(&path)
            .await
            .expect("unreadable lines must not stop the job store from opening");
        let job = store.get(&id).await.unwrap().unwrap();
        assert_eq!(job.name, "keep");
        let health = JournalJobStore::inspect(&path).await.unwrap();
        assert_eq!(health.incompatible_records, 0);
        assert_eq!(job.status, JobStatus::ReconciliationRequired);
        assert_eq!(
            JournalJobStore::inspect(archived_journal(&path))
                .await
                .unwrap()
                .incompatible_records,
            2
        );
    });
}

#[test]
fn damaged_job_history_never_replays_an_older_pending_record() {
    let rt = runtime();
    rt.block_on(async {
        for unsupported_schema in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("jobs.jsonl");
            let store = JournalJobStore::open(&path).await.unwrap();
            let job = Job::new("echo".into(), serde_json::json!({}), JobPriority::Normal);
            store.put(&job).await.unwrap();
            drop(store);
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            if unsupported_schema {
                let record = serde_json::json!({"schemaVersion":99,"sequence":u64::MAX,
                    "recordedAt":chrono::Utc::now(),"event":"started","job":job});
                writeln!(file, "{record}").unwrap();
            } else {
                writeln!(file, "damaged started transition").unwrap();
            }
            drop(file);
            let original = std::fs::read(&path).unwrap();
            for _ in 0..2 {
                let store = JournalJobStore::open(&path).await.unwrap();
                assert!(
                    store
                        .pending()
                        .await
                        .unwrap()
                        .iter()
                        .all(|candidate| candidate.id != job.id),
                    "uncertain jobs must not replay"
                );
                assert_eq!(
                    store.get(&job.id).await.unwrap().unwrap().status,
                    JobStatus::ReconciliationRequired
                );
                assert!(store
                    .put(&Job::new(
                        "echo".into(),
                        serde_json::json!({}),
                        JobPriority::Normal
                    ))
                    .await
                    .is_ok());
                assert_eq!(
                    std::fs::read(archived_journal(&path)).unwrap(),
                    original,
                    "inspection must preserve damaged evidence"
                );
            }
        }
    });
}

#[test]
fn job_resolution_is_owner_scoped_durable_and_never_requeues() {
    runtime().block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.jsonl");
        let owner = types::PrincipalId::from_uuid(uuid::Uuid::new_v4());
        let store = JournalJobStore::open(&path).await.unwrap();
        let mut job = Job::new("echo".into(), serde_json::json!({}), JobPriority::Normal)
            .with_owner(owner.clone());
        job.start();
        store.put(&job).await.unwrap();
        drop(store);
        let scheduler = JobScheduler::open_journal(SchedulerConfig::default(), &path)
            .await
            .unwrap();
        let input: types::JobResolutionRequest = serde_json::from_value(serde_json::json!({
            "decision":"effectObserved", "evidenceSha256":"a".repeat(64),
        }))
        .unwrap();
        let intruder = types::PrincipalId::from_uuid(uuid::Uuid::new_v4());
        assert!(scheduler
            .resolve_job(&job.id, &intruder, input.clone())
            .await
            .is_err());
        let receipt = scheduler
            .resolve_job(&job.id, &owner, input.clone())
            .await
            .unwrap();
        assert_eq!(receipt.provenance, "operatorAttested");
        assert_eq!(
            scheduler.get_job(&job.id).await.unwrap().status.to_string(),
            "resolved"
        );
        assert_eq!(scheduler.stats().await.queued_jobs, 0);
        assert_eq!(
            scheduler
                .resolve_job(&job.id, &owner, input.clone())
                .await
                .unwrap(),
            receipt
        );
        let mut conflict = input.clone();
        conflict.evidence_sha256 = "b".repeat(64);
        assert!(scheduler
            .resolve_job(&job.id, &owner, conflict)
            .await
            .is_err());
        drop(scheduler);
        let scheduler = JobScheduler::open_journal(SchedulerConfig::default(), &path)
            .await
            .unwrap();
        assert_eq!(
            scheduler.resolve_job(&job.id, &owner, input).await.unwrap(),
            receipt
        );
        assert_eq!(scheduler.stats().await.queued_jobs, 0);
    });
}

#[test]
fn inspect_reports_torn_tail_without_truncating() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("jobs.jsonl");
    let rt = runtime();

    rt.block_on(async {
        let scheduler = JobScheduler::open_journal(SchedulerConfig::default(), &path)
            .await
            .unwrap();
        scheduler
            .submit(JobConfig::new("keep".to_string(), serde_json::json!({})))
            .await
            .unwrap();
    });

    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(f, "{{\"schemaVersion\":1,\"sequence\":99,\"recordedAt\":").unwrap();
        f.flush().unwrap();
    }

    let before = std::fs::read(&path).unwrap();
    let health = rt.block_on(async { JournalJobStore::inspect(&path).await.unwrap() });
    assert!(health.exists);
    assert!(health.torn_tail);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn inspect_missing_scheduler_journal_is_empty_health() {
    let dir = tempfile::tempdir().unwrap();
    let health = runtime().block_on(async {
        JournalJobStore::inspect(dir.path().join("nope.jsonl"))
            .await
            .unwrap()
    });
    assert!(!health.exists);
    assert_eq!(health.bytes, 0);
    assert!(!health.torn_tail);
}

#[test]
fn inspection_preserves_large_records_blank_lines_and_damaged_tail() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("jobs.jsonl");
    let job = Job::new(
        "large".into(),
        serde_json::json!({ "text": "x".repeat(20_000) }),
        JobPriority::Normal,
    );
    let mut record = task_scheduler::JournalRecord {
        schema_version: 1,
        sequence: 0,
        recorded_at: chrono::Utc::now(),
        event: JobEvent::Submitted,
        job,
    };
    let mut bytes = b"\n\n".to_vec();
    serde_json::to_writer(&mut bytes, &record).unwrap();
    bytes.extend_from_slice(b"\n\n");
    record.schema_version = u16::MAX;
    serde_json::to_writer(&mut bytes, &record).unwrap();
    bytes.extend_from_slice(b"\n\xff\n{\"schemaVersion\":1");
    std::fs::write(&path, &bytes).unwrap();

    let health = runtime().block_on(JournalJobStore::inspect(&path)).unwrap();
    assert!(health.exists);
    assert_eq!(health.bytes, bytes.len() as u64);
    assert_eq!(health.records, 3);
    assert_eq!(health.incompatible_records, 2);
    assert!(health.torn_tail);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
fn cancel_before_handler_first_poll_releases_active_slot() {
    use std::future::Future;
    use std::task::Poll;

    runtime().block_on(async {
        let calls = Arc::new(AtomicU32::new(0));
        struct CountingHandler(Arc<AtomicU32>);
        #[async_trait]
        impl JobHandler for CountingHandler {
            async fn execute(&self, _job: &Job) -> Result<serde_json::Value, String> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(serde_json::json!({}))
            }
        }
        let mut scheduler = JobScheduler::new(SchedulerConfig::default());
        scheduler.register_handler("test".into(), Arc::new(CountingHandler(calls.clone())));
        let id = scheduler
            .submit(JobConfig::new("test".into(), serde_json::json!({})))
            .await
            .unwrap();

        // Poll only the runner, leaving its spawned handler unpolled on this
        // current-thread executor before cancellation.
        let mut runner = Box::pin(scheduler.run());
        std::future::poll_fn(|cx| {
            assert!(runner.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(scheduler.stats().await.active_jobs, 1);
        scheduler.cancel_job(&id).await.unwrap();
        scheduler.request_shutdown();
        runner.await.unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(scheduler.stats().await.active_jobs, 0);
        assert_eq!(
            scheduler.get_job(&id).await.unwrap().status,
            JobStatus::ReconciliationRequired
        );
    });
}

#[test]
fn cancel_aborts_running() {
    let mut scheduler = JobScheduler::new(
        SchedulerConfig::default()
            .with_job_timeout(30_000)
            .with_backoff_range(1, 5)
            .with_drain_timeout(2_000),
    );
    scheduler.register_handler("hang".to_string(), Arc::new(HangHandler));
    let scheduler = Arc::new(scheduler);
    let rt = runtime();

    rt.block_on(async {
        let id = scheduler
            .submit(JobConfig::new("hang".to_string(), serde_json::json!({})))
            .await
            .unwrap();

        let runner = {
            let s = Arc::clone(&scheduler);
            tokio::spawn(async move { s.run().await })
        };

        wait_status(&scheduler, &id, JobStatus::Running, 50).await;
        scheduler.cancel_job(&id).await.unwrap();

        let job = wait_status(&scheduler, &id, JobStatus::ReconciliationRequired, 50).await;
        assert_eq!(job.status, JobStatus::ReconciliationRequired);

        for _ in 0..50 {
            if scheduler.stats().await.active_jobs == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(scheduler.stats().await.active_jobs, 0);

        scheduler.request_shutdown();
        let _ = runner.await.unwrap();
    });
}

#[test]
fn failed_pending_cancel_remains_pending_and_runnable() {
    runtime().block_on(async {
        let store = Arc::new(RejectEventStore {
            inner: MemoryJobStore::new(),
            denied: JobEvent::Cancelled,
        });
        let scheduler = JobScheduler::with_store(SchedulerConfig::default(), store);
        let id = scheduler
            .submit(JobConfig::new("test".into(), serde_json::json!({})))
            .await
            .unwrap();
        assert!(scheduler.cancel_job(&id).await.is_err());
        assert_eq!(
            scheduler.get_job(&id).await.unwrap().status,
            JobStatus::Pending
        );
        assert_eq!(scheduler.stats().await.queued_jobs, 1);
    });
}

#[test]
fn retry_does_not_hold_permit() {
    let mut scheduler = JobScheduler::new(
        SchedulerConfig::default()
            .with_max_concurrent(1)
            .with_job_timeout(5_000)
            .with_backoff_range(200, 200)
            .with_drain_timeout(5_000),
    );
    scheduler.register_handler(
        "flaky".to_string(),
        Arc::new(FailNTimes {
            failures_remaining: AtomicU32::new(1),
        }),
    );
    scheduler.register_handler("fast".to_string(), Arc::new(OkHandler));
    let scheduler = Arc::new(scheduler);
    let rt = runtime();

    rt.block_on(async {
        let flaky_id = scheduler
            .submit(JobConfig::new("flaky".to_string(), serde_json::json!({})).with_max_retries(3))
            .await
            .unwrap();
        let fast_id = scheduler
            .submit(JobConfig::new("fast".to_string(), serde_json::json!({})))
            .await
            .unwrap();

        let runner = {
            let s = Arc::clone(&scheduler);
            tokio::spawn(async move { s.run().await })
        };

        // Fast job must complete while flaky is in backoff (permit released).
        let fast = wait_status(&scheduler, &fast_id, JobStatus::Completed, 100).await;
        assert!(fast.result.as_ref().unwrap().success);

        let flaky = wait_status(&scheduler, &flaky_id, JobStatus::Completed, 150).await;
        assert_eq!(flaky.retry_count, 1);

        scheduler.request_shutdown();
        runner.await.unwrap().unwrap();
    });
}

#[test]
fn drain_deadline_aborts() {
    let mut scheduler = JobScheduler::new(
        SchedulerConfig::default()
            .with_job_timeout(60_000)
            .with_backoff_range(1, 5)
            .with_drain_timeout(80),
    );
    scheduler.register_handler("hang".to_string(), Arc::new(HangHandler));
    let scheduler = Arc::new(scheduler);
    let rt = runtime();

    rt.block_on(async {
        let id = scheduler
            .submit(JobConfig::new("hang".to_string(), serde_json::json!({})))
            .await
            .unwrap();

        let runner = {
            let s = Arc::clone(&scheduler);
            tokio::spawn(async move { s.run_with_drain(Duration::from_millis(80)).await })
        };

        // Let the hanging job start
        tokio::time::sleep(Duration::from_millis(40)).await;
        scheduler.request_shutdown();

        let result = tokio::time::timeout(Duration::from_secs(3), runner)
            .await
            .expect("run returns within drain window")
            .unwrap();
        assert_eq!(result, Err(JobError::DrainTimeout));
        assert_eq!(
            scheduler.get_job(&id).await.unwrap().status,
            JobStatus::ReconciliationRequired,
        );
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_thread_smoke() {
    let mut scheduler = JobScheduler::new(
        SchedulerConfig::default()
            .with_max_concurrent(4)
            .with_job_timeout(5_000)
            .with_backoff_range(1, 5)
            .with_drain_timeout(5_000),
    );
    scheduler.register_handler("test".to_string(), Arc::new(OkHandler));
    let scheduler = Arc::new(scheduler);

    let mut ids = Vec::new();
    for i in 0..20 {
        let id = scheduler
            .submit(JobConfig::new(
                "test".to_string(),
                serde_json::json!({"i": i}),
            ))
            .await
            .unwrap();
        ids.push(id);
    }

    let runner = {
        let s = Arc::clone(&scheduler);
        tokio::spawn(async move { s.run().await })
    };

    for id in &ids {
        wait_status(&scheduler, id, JobStatus::Completed, 200).await;
    }

    let stats = scheduler.stats().await;
    assert_eq!(stats.total_completed, 20);
    assert_eq!(stats.total_submitted, 20);

    scheduler.request_shutdown();
    runner.await.unwrap().unwrap();
}

#[test]
fn terminal_jobs_are_pruned_beyond_the_retention_bound() {
    let rt = runtime();
    rt.block_on(async {
        let mut config = SchedulerConfig::default().with_backoff_range(1, 5);
        config.retained_terminal_jobs = 2;
        let mut scheduler = JobScheduler::from_config(config).await.unwrap();
        scheduler.register_handler("echo".to_string(), Arc::new(OkHandler));
        let scheduler = Arc::new(scheduler);
        let run = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.run().await })
        };

        let mut ids = Vec::new();
        for index in 0..5 {
            ids.push(
                scheduler
                    .submit(JobConfig::new(
                        "echo".to_string(),
                        serde_json::json!({"index": index}),
                    ))
                    .await
                    .unwrap(),
            );
        }
        // Wait only for the last job: earlier completions may already be
        // pruned by the retention bound being exercised here.
        wait_status(&scheduler, ids.last().unwrap(), JobStatus::Completed, 50).await;
        let mut completed_visible = 0;
        for id in &ids {
            if let Some(job) = scheduler.get_job(id).await {
                if job.status == JobStatus::Completed {
                    completed_visible += 1;
                }
            }
        }
        if completed_visible > 2 {
            for id in &ids {
                if let Some(job) = scheduler.get_job(id).await {
                    eprintln!("visible: {:?} {:?}", id, job.status);
                } else {
                    eprintln!("missing: {:?}", id);
                }
            }
            panic!("registry retained {completed_visible} terminal jobs over the bound of 2");
        }

        scheduler.request_shutdown();
        let _ = run.await;
    });
}

#[test]
fn oversized_journal_retains_newest_terminal_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("jobs.jsonl");
    let rt = runtime();
    rt.block_on(async {
        // Bulk-write more records than the compaction threshold without
        // paying the per-record fsync of real submits.
        let mut text = String::new();
        for index in 0..4_200_u64 {
            let created_at = chrono::DateTime::from_timestamp(index as i64, 0)
                .unwrap()
                .to_rfc3339();
            let job = serde_json::json!({
                "id": format!("job-{index}"),
                "name": "echo",
                "priority": "Normal",
                "status": if index % 21 == 0 { "Pending" } else { "Completed" },
                "payload": {},
                "createdAt": created_at,
                "startedAt": null,
                "completedAt": null,
                "retryCount": 0,
                "maxRetries": 3,
                "result": null,
                "error": null
            });
            let record = serde_json::json!({
                "schemaVersion": 1,
                "sequence": index,
                "recordedAt": "2026-08-05T00:00:00Z",
                "event": "submitted",
                "job": job,
            });
            text.push_str(&serde_json::to_string(&record).unwrap());
            text.push('\n');
        }
        tokio::fs::write(&path, text).await.unwrap();
        let before = tokio::fs::metadata(&path).await.unwrap().len();

        let _first = JournalJobStore::open(&path).await.unwrap();
        let after = tokio::fs::metadata(&path).await.unwrap().len();
        assert!(
            after < before / 2,
            "journal was not compacted: {before} -> {after}"
        );
        // The first open's in-memory index holds everything it scanned;
        // compaction bounds the FILE. Re-open to read the compacted file.
        let store = JournalJobStore::open(&path).await.unwrap();
        let jobs = store.load_all().await.unwrap();
        assert!(
            jobs.iter().any(|job| job.status == JobStatus::Pending),
            "pending jobs must survive compaction"
        );
        assert!(
            jobs.iter()
                .filter(|job| job.status == JobStatus::Completed)
                .count()
                <= 1024,
            "terminal history must be bounded by compaction"
        );
        assert!(
            jobs.iter().any(|job| job.id.0 == "job-4199"),
            "the newest terminal job must survive compaction"
        );
        assert!(
            !jobs.iter().any(|job| job.id.0 == "job-1"),
            "the oldest terminal job must be pruned"
        );
    });
}

#[test]
fn failed_resolution_persistence_leaves_uncertainty_and_never_admits_work() {
    runtime().block_on(async {
        let store = Arc::new(RejectEventStore {
            inner: MemoryJobStore::new(),
            denied: JobEvent::Resolved,
        });
        let owner = types::PrincipalId::from_uuid(uuid::Uuid::new_v4());
        let mut job = Job::new("echo".into(), serde_json::json!({}), JobPriority::Normal)
            .with_owner(owner.clone());
        job.require_reconciliation("uncertain effect");
        store.put(&job).await.unwrap();
        let scheduler = JobScheduler::with_store(SchedulerConfig::default(), store);
        scheduler.hydrate().await.unwrap();
        assert!(scheduler
            .resolve_job(
                &job.id,
                &owner,
                types::JobResolutionRequest {
                    decision: types::JobResolutionDecision::EffectAbsent,
                    evidence_sha256: "a".repeat(64)
                }
            )
            .await
            .is_err());
        let still_uncertain = scheduler.get_job(&job.id).await.unwrap();
        assert_eq!(still_uncertain.status, JobStatus::ReconciliationRequired);
        assert!(still_uncertain.resolution.is_none());
        assert_eq!(scheduler.stats().await.queued_jobs, 0);
        scheduler.deny_admission("unreadableLedger");
        assert_eq!(
            scheduler
                .clone()
                .submit(JobConfig::new("echo".into(), serde_json::json!({})))
                .await
                .unwrap_err(),
            JobError::Integrity
        );
        assert_eq!(scheduler.stats().await.queued_jobs, 0);
    });
}

struct DelayedEventStore {
    inner: MemoryJobStore,
    event: JobEvent,
    first: std::sync::atomic::AtomicBool,
    append_before_wait: bool,
    fail_after_append: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait]
impl JobStore for DelayedEventStore {
    async fn put(&self, job: &Job) -> Result<(), StoreError> {
        self.inner.put(job).await
    }
    async fn get(&self, id: &JobId) -> Result<Option<Job>, StoreError> {
        self.inner.get(id).await
    }
    async fn update(&self, job: &Job, event: JobEvent) -> Result<(), StoreError> {
        if event == self.event && self.first.swap(false, Ordering::SeqCst) {
            if self.append_before_wait {
                self.inner.update(job, event).await?;
            }
            self.entered.notify_one();
            self.release.notified().await;
            if self.fail_after_append.swap(false, Ordering::SeqCst) {
                return Err(std::io::Error::other("acknowledgment lost after append").into());
            }
        }
        self.inner.update(job, event).await
    }
    async fn pending(&self) -> Result<Vec<Job>, StoreError> {
        self.inner.pending().await
    }
    async fn load_all(&self) -> Result<Vec<Job>, StoreError> {
        self.inner.load_all().await
    }
}
fn delayed_store(event: JobEvent, append_before_wait: bool) -> Arc<DelayedEventStore> {
    Arc::new(DelayedEventStore {
        inner: MemoryJobStore::new(),
        event,
        first: std::sync::atomic::AtomicBool::new(true),
        append_before_wait,
        fail_after_append: std::sync::atomic::AtomicBool::new(false),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    })
}
#[test]
fn resolution_waits_for_older_timeout_transition_to_be_durable() {
    runtime().block_on(async {
        let store = delayed_store(JobEvent::Recovered, false);
        let owner = types::PrincipalId::from_uuid(uuid::Uuid::new_v4());
        let mut scheduler = JobScheduler::with_store(
            SchedulerConfig::default().with_job_timeout(10),
            store.clone(),
        );
        scheduler.register_handler("hang".into(), Arc::new(HangHandler));
        let id = scheduler
            .submit(JobConfig::new("hang".into(), serde_json::json!({})).with_owner(owner.clone()))
            .await
            .unwrap();
        let runner_scheduler = scheduler.clone();
        let runner = tokio::spawn(async move { runner_scheduler.run().await });
        tokio::time::timeout(Duration::from_secs(2), store.entered.notified())
            .await
            .unwrap();
        let resolver = scheduler.clone();
        let resolver_id = id.clone();
        let mut task = tokio::spawn(async move {
            resolver
                .resolve_job(
                    &resolver_id,
                    &owner,
                    types::JobResolutionRequest {
                        decision: types::JobResolutionDecision::EffectAbsent,
                        evidence_sha256: "a".repeat(64),
                    },
                )
                .await
        });
        let waited = tokio::time::timeout(Duration::from_millis(50), &mut task)
            .await
            .is_err();
        store.release.notify_one();
        if waited {
            task.await.unwrap().unwrap();
        }
        scheduler.request_shutdown();
        runner.await.unwrap().unwrap();
        assert!(
            waited,
            "a resolution must wait for the older uncertain transition"
        );
        assert_eq!(
            store.get(&id).await.unwrap().unwrap().status,
            JobStatus::Resolved
        );
    });
}
#[test]
fn cancelled_resolution_after_append_cannot_accept_a_conflicting_attestation() {
    runtime().block_on(async {
        let store = delayed_store(JobEvent::Resolved, true);
        let owner = types::PrincipalId::from_uuid(uuid::Uuid::new_v4());
        let mut job = Job::new("echo".into(), serde_json::json!({}), JobPriority::Normal)
            .with_owner(owner.clone());
        job.require_reconciliation("uncertain effect");
        store.put(&job).await.unwrap();
        let scheduler = JobScheduler::with_store(SchedulerConfig::default(), store.clone());
        scheduler.hydrate().await.unwrap();
        let resolver = scheduler.clone();
        let id = job.id.clone();
        let actor = owner.clone();
        let task = tokio::spawn(async move {
            resolver
                .resolve_job(
                    &id,
                    &actor,
                    types::JobResolutionRequest {
                        decision: types::JobResolutionDecision::EffectObserved,
                        evidence_sha256: "a".repeat(64),
                    },
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), store.entered.notified())
            .await
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let conflict = scheduler
            .resolve_job(
                &job.id,
                &owner,
                types::JobResolutionRequest {
                    decision: types::JobResolutionDecision::EffectAbsent,
                    evidence_sha256: "b".repeat(64),
                },
            )
            .await;
        assert!(
            conflict.is_err(),
            "an uncertain write must not be overwritten in this process"
        );
        assert_eq!(
            store
                .get(&job.id)
                .await
                .unwrap()
                .unwrap()
                .resolution
                .unwrap()
                .evidence_sha256,
            "a".repeat(64)
        );
    });
}

#[test]
fn malformed_stored_resolution_is_degraded_before_hydration() {
    runtime().block_on(async {
        let owner = types::PrincipalId::from_uuid(uuid::Uuid::new_v4());
        let mut job = Job::new("echo".into(), serde_json::json!({}), JobPriority::Normal).with_owner(owner.clone());
        let time = chrono::Utc::now();
        job.status = JobStatus::Resolved; job.completed_at = Some(time);
        job.resolution = Some(types::JobResolutionReceipt { job_id: job.id.0.clone(), actor: owner, resolved_at: time, decision: types::JobResolutionDecision::EffectAbsent, evidence_sha256: "a".repeat(64), provenance: "operatorAttested".into() });
        for (field, value) in [("evidenceSha256", serde_json::json!("bad")), ("provenance", serde_json::json!("runtimeVerified")), ("jobId", serde_json::json!(JobId::new().0)), ("actor", serde_json::json!(uuid::Uuid::new_v4())), ("resolvedAt", serde_json::json!(time + chrono::Duration::seconds(1))), ("status", serde_json::json!("pending")), ("resolution", serde_json::Value::Null)] {
            let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("jobs.jsonl");
            let mut record = serde_json::json!({"schemaVersion":1,"sequence":0,"recordedAt":time,"event":"resolved","job":job});
            if field == "status" || field == "resolution" { record["job"][field] = value; } else { record["job"]["resolution"][field] = value; }
            let original = format!("{record}\n"); std::fs::write(&path, &original).unwrap();
            let store = JournalJobStore::open(&path).await.unwrap();
            assert!(store.integrity_issue().is_none(), "invalid {field} must be archived rather than block new work");
            assert!(store.pending().await.unwrap().is_empty());
            assert_eq!(std::fs::read_to_string(archived_journal(&path)).unwrap(), original);
        }
    });
}

#[test]
fn resolution_acknowledgment_error_requires_reload_before_conflicting_retry() {
    runtime().block_on(async {
        let store = delayed_store(JobEvent::Resolved, true);
        store.fail_after_append.store(true, Ordering::SeqCst);
        store.release.notify_one();
        let owner = types::PrincipalId::from_uuid(uuid::Uuid::new_v4());
        let mut job = Job::new("echo".into(), serde_json::json!({}), JobPriority::Normal)
            .with_owner(owner.clone());
        job.require_reconciliation("uncertain effect");
        store.put(&job).await.unwrap();
        let scheduler = JobScheduler::with_store(SchedulerConfig::default(), store.clone());
        scheduler.hydrate().await.unwrap();
        let input = types::JobResolutionRequest {
            decision: types::JobResolutionDecision::EffectObserved,
            evidence_sha256: "a".repeat(64),
        };
        assert_eq!(
            scheduler
                .resolve_job(&job.id, &owner, input.clone())
                .await
                .unwrap_err(),
            JobError::ResolutionUncertain
        );
        assert!(scheduler
            .get_job(&job.id)
            .await
            .unwrap()
            .resolution
            .is_none());
        let conflict = types::JobResolutionRequest {
            decision: types::JobResolutionDecision::EffectAbsent,
            evidence_sha256: "b".repeat(64),
        };
        assert_eq!(
            scheduler
                .resolve_job(&job.id, &owner, conflict)
                .await
                .unwrap_err(),
            JobError::ResolutionUncertain
        );
        let reloaded = JobScheduler::with_store(SchedulerConfig::default(), store);
        reloaded.hydrate().await.unwrap();
        let receipt = reloaded.resolve_job(&job.id, &owner, input).await.unwrap();
        assert_eq!(receipt.evidence_sha256, "a".repeat(64));
    });
}

#[tokio::test]
async fn durable_pruning_survives_restart_and_keeps_pending_work() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("jobs.jsonl");
    let store = JournalJobStore::open(&path).await.unwrap();
    let pending = Job::new("pending".into(), serde_json::json!({}), JobPriority::Normal);
    store.put(&pending).await.unwrap();
    for _ in 0..5 {
        let mut job = Job::new(
            "terminal".into(),
            serde_json::json!({}),
            JobPriority::Normal,
        );
        job.fail("finished".into());
        store.put(&job).await.unwrap();
    }
    store.prune_terminal(2).await.unwrap();
    assert_eq!(store.load_all().await.unwrap().len(), 3);
    drop(store);
    let reopened = JournalJobStore::open(&path).await.unwrap();
    assert_eq!(reopened.load_all().await.unwrap().len(), 3);
    assert_eq!(reopened.pending().await.unwrap()[0].id, pending.id);
}

#[cfg(unix)]
#[tokio::test]
async fn compaction_never_overwrites_a_preexisting_temporary_path() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("jobs.jsonl");
    let unrelated = root.path().join("unrelated.txt");
    tokio::fs::write(&unrelated, b"preserve these bytes")
        .await
        .unwrap();
    std::os::unix::fs::symlink(&unrelated, path.with_extension("compact.tmp")).unwrap();
    let store = JournalJobStore::open(&path).await.unwrap();
    let mut job = Job::new(
        "terminal".into(),
        serde_json::json!({}),
        JobPriority::Normal,
    );
    job.fail("finished".into());
    store.put(&job).await.unwrap();
    store.prune_terminal(0).await.unwrap();
    assert_eq!(
        tokio::fs::read(&unrelated).await.unwrap(),
        b"preserve these bytes"
    );
    assert!(tokio::fs::symlink_metadata(&path)
        .await
        .unwrap()
        .file_type()
        .is_file());
}

#[tokio::test]
async fn online_compaction_keeps_appending_to_the_published_journal() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("jobs.jsonl");
    let mut job = Job::new("before".into(), serde_json::json!({}), JobPriority::Normal);
    let mut bytes = Vec::new();
    // Below the startup threshold, but the next update crosses the online
    // threshold. Bulk fixture construction avoids thousands of fsync calls.
    for sequence in 0..4096 {
        serde_json::to_writer(
            &mut bytes,
            &task_scheduler::JournalRecord {
                schema_version: 1,
                sequence,
                recorded_at: chrono::Utc::now(),
                event: JobEvent::Submitted,
                job: job.clone(),
            },
        )
        .unwrap();
        bytes.push(b'\n');
    }
    tokio::fs::write(&path, &bytes).await.unwrap();
    let store = JournalJobStore::open(&path).await.unwrap();
    job.name = "latest".into();
    store.update(&job, JobEvent::Retried).await.unwrap();
    assert!(tokio::fs::metadata(&path).await.unwrap().len() < bytes.len() as u64 / 100);
    let next = Job::new("after".into(), serde_json::json!({}), JobPriority::Normal);
    store.put(&next).await.unwrap();
    drop(store);
    let reopened = JournalJobStore::open(&path).await.unwrap();
    assert_eq!(reopened.get(&job.id).await.unwrap().unwrap().name, "latest");
    assert!(reopened.get(&next.id).await.unwrap().is_some());
    let text = tokio::fs::read_to_string(&path).await.unwrap();
    let sequences: Vec<_> = text
        .lines()
        .map(|line| {
            serde_json::from_str::<task_scheduler::JournalRecord>(line)
                .unwrap()
                .sequence
        })
        .collect();
    assert_eq!(sequences, vec![0, 1]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
