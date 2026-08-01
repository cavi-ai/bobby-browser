use async_trait::async_trait;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use task_scheduler::{
    Job, JobConfig, JobError, JobHandler, JobId, JobPriority, JobQueue, JobResult, JobScheduler,
    JobStatus, RetryConfig, SchedulerConfig,
};
use tokio::sync::Mutex;

// ===== Job tests =====

#[test]
fn job_new_has_pending_status() {
    let job = Job::new("test".to_string(), serde_json::json!({}), JobPriority::Normal);
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
    let mut job = Job::new("test".to_string(), serde_json::json!({}), JobPriority::Normal);
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
    let mut job = Job::new("test".to_string(), serde_json::json!({}), JobPriority::Normal)
        .with_max_retries(3);
    job.fail("error".to_string());
    assert!(job.can_retry());
}

#[test]
fn job_cannot_retry_when_max_retries_exceeded() {
    let mut job = Job::new("test".to_string(), serde_json::json!({}), JobPriority::Normal)
        .with_max_retries(0);
    job.fail("error".to_string());
    assert!(!job.can_retry());
}

#[test]
fn job_increment_retry() {
    let mut job = Job::new("test".to_string(), serde_json::json!({}), JobPriority::Normal);
    assert_eq!(job.retry_count, 0);
    job.increment_retry();
    assert_eq!(job.retry_count, 1);
    job.increment_retry();
    assert_eq!(job.retry_count, 2);
}

#[test]
fn job_prepare_retry_resets_to_pending() {
    let mut job = Job::new("test".to_string(), serde_json::json!({}), JobPriority::Normal)
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
        .submit(
            JobConfig::new("test".to_string(), serde_json::json!({}))
                .with_max_retries(3),
        )
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
            .submit(
                JobConfig::new("flaky".to_string(), serde_json::json!({}))
                    .with_max_retries(3),
            )
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
            .with_max_retries(0),
    );
    scheduler.register_handler("hang".to_string(), Arc::new(HangHandler));
    let scheduler = Arc::new(scheduler);
    let rt = runtime();

    rt.block_on(async {
        let id = scheduler
            .submit(
                JobConfig::new("hang".to_string(), serde_json::json!({}))
                    .with_max_retries(0),
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
        assert!(
            job.error
                .as_deref()
                .is_some_and(|e| e.contains("timeout") || e.contains("job timeout"))
        );

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
                JobConfig::new("missing".to_string(), serde_json::json!({}))
                    .with_max_retries(0),
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
}

#[test]
fn scheduler_config_chain() {
    let config = SchedulerConfig::default()
        .with_max_concurrent(5)
        .with_queue_size(200)
        .with_max_retries(5)
        .with_backoff_range(500, 30000)
        .with_job_timeout(60000);

    assert_eq!(config.max_concurrent_jobs, 5);
    assert_eq!(config.max_queue_size, 200);
    assert_eq!(config.max_retries, 5);
    assert_eq!(config.retry_backoff_base_ms, 500);
    assert_eq!(config.retry_backoff_max_ms, 30000);
    assert_eq!(config.job_timeout_ms, 60000);
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
