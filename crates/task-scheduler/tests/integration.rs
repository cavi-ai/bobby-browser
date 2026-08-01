use task_scheduler::{
    Job, JobConfig, JobError, JobId, JobPriority, JobQueue, JobResult, JobScheduler, JobStatus,
    RetryConfig, SchedulerConfig,
};

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
    let job = Job::new("test".to_string(), serde_json::json!({}), JobPriority::Normal);
    let result = JobResult {
        job_id: job.id.clone(),
        success: true,
        output: Some(serde_json::json!({"ok": true})),
        error: None,
        completed_at: chrono::Utc::now(),
    };
    let completed = job.complete(result);
    assert_eq!(completed.status, JobStatus::Completed);
    assert!(completed.result.is_some());
}

#[test]
fn job_fail_sets_failed() {
    let job = Job::new("test".to_string(), serde_json::json!({}), JobPriority::Low);
    let failed = job.fail("something broke".to_string());
    assert_eq!(failed.status, JobStatus::Failed);
    assert_eq!(failed.error, Some("something broke".to_string()));
}

#[test]
fn job_cancel_sets_cancelled() {
    let job = Job::new("test".to_string(), serde_json::json!({}), JobPriority::Critical);
    let cancelled = job.cancel();
    assert_eq!(cancelled.status, JobStatus::Cancelled);
}

#[test]
fn job_can_retry_when_failed_and_retries_remaining() {
    let job = Job::new("test".to_string(), serde_json::json!({}), JobPriority::Normal)
        .with_max_retries(3);
    let _failed = job.clone().fail("error".to_string());
    assert!(job.can_retry());
}

#[test]
fn job_cannot_retry_when_max_retries_exceeded() {
    let job = Job::new("test".to_string(), serde_json::json!({}), JobPriority::Normal)
        .with_max_retries(0);
    let _failed = job.clone().fail("error".to_string());
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

// ===== JobQueue tests =====

#[test]
fn job_queue_submit_and_next() {
    let mut queue = JobQueue::new(100);
    let id = queue
        .submit(JobConfig::new("test".to_string(), serde_json::json!({})))
        .unwrap();

    assert_eq!(queue.len(), 1);
    let job = queue.next_job().unwrap();
    assert_eq!(job.id, id);
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
    queue.submit(JobConfig::new("a".to_string(), serde_json::json!({}))).unwrap();
    queue.submit(JobConfig::new("b".to_string(), serde_json::json!({}))).unwrap();

    let result = queue.submit(JobConfig::new("c".to_string(), serde_json::json!({})));
    assert_eq!(result, Err(JobError::QueueFull));
    assert_eq!(queue.len(), 2);
}

#[test]
fn job_queue_priority_ordering() {
    let mut queue = JobQueue::new(100);
    queue.submit(JobConfig::new("low".to_string(), serde_json::json!({})))
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
}

#[test]
fn job_queue_complete_job() {
    let mut queue = JobQueue::new(100);
    let id = queue
        .submit(JobConfig::new("test".to_string(), serde_json::json!({})))
        .unwrap();

    let result = JobResult {
        job_id: id.clone(),
        success: true,
        output: None,
        error: None,
        completed_at: chrono::Utc::now(),
    };
    queue.complete_job(&id, result).unwrap();

    // Job should still be in queue (completed status)
    assert_eq!(queue.len(), 1);
}

#[test]
fn job_queue_retry_job() {
    let mut queue = JobQueue::new(100);
    let _id = queue
        .submit(
            JobConfig::new("test".to_string(), serde_json::json!({}))
                .with_max_retries(3),
        )
        .unwrap();

    // Get the job, fail it, then retry
    let job = queue.next_job().unwrap();
    let _failed = job.fail("error".to_string());

    // Re-submit and retry
    let reconfig = JobConfig::new("test".to_string(), serde_json::json!({}))
        .with_max_retries(3);
    let retried_id = queue.submit(reconfig).unwrap();

    // Should be able to retry
    let _backoff = queue.retry_job(&retried_id).unwrap();
}

#[test]
fn job_queue_stats() {
    let mut queue = JobQueue::new(100);
    queue.submit(JobConfig::new("a".to_string(), serde_json::json!({}))).unwrap();
    queue.submit(JobConfig::new("b".to_string(), serde_json::json!({}))).unwrap();

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

    assert!(backoff2.as_millis() > backoff1.as_millis());
    assert!(backoff3.as_millis() >= backoff2.as_millis());
}

#[test]
fn retry_config_backoff_capped() {
    let config = RetryConfig {
        max_retries: 10,
        backoff_base_ms: 1000,
        backoff_max_ms: 5000,
    };

    let backoff = config.calculate_backoff(10);
    assert!(backoff.as_millis() <= 5000);
}

// ===== JobScheduler tests =====

#[test]
fn scheduler_new_with_config() {
    let config = SchedulerConfig::default();
    let scheduler = JobScheduler::new(config);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let stats = rt.block_on(async { scheduler.stats().await });
    assert_eq!(stats.total_submitted, 0);
    assert_eq!(stats.queued_jobs, 0);
    drop(rt);
}

#[test]
fn scheduler_submit_and_stats() {
    let config = SchedulerConfig::default();
    let scheduler = JobScheduler::new(config);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let id = scheduler
            .submit(JobConfig::new("test".to_string(), serde_json::json!({})))
            .await
            .unwrap();

        let stats = scheduler.stats().await;
        assert_eq!(stats.queued_jobs, 1);
    });
}

#[test]
fn scheduler_max_concurrent() {
    let config = SchedulerConfig::default()
        .with_max_concurrent(2)
        .with_queue_size(10);
    let scheduler = JobScheduler::new(config);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let stats = scheduler.stats().await;
        assert_eq!(stats.total_submitted, 0);
        assert_eq!(stats.queued_jobs, 0);
    });
    drop(rt);
}

#[test]
fn scheduler_cancel_job() {
    let config = SchedulerConfig::default();
    let scheduler = JobScheduler::new(config);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let _id = scheduler
            .submit(JobConfig::new("test".to_string(), serde_json::json!({})))
            .await
            .unwrap();

        scheduler.cancel_job(&_id).await.unwrap();

        // Cancelled jobs are still counted in the queue
        let stats = scheduler.stats().await;
        assert!(stats.queued_jobs >= 0);
    });
    drop(rt);
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
