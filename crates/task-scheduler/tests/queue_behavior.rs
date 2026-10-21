use task_scheduler::{JobConfig, JobError, JobPriority, JobQueue, JobResult};

#[test]
fn job_queue_rejected_cancellation_preserves_completed_job() {
    let mut queue = JobQueue::new(1);
    let job = queue
        .submit(JobConfig::new("finished".into(), serde_json::json!({})))
        .unwrap();
    queue
        .complete_job(
            &job.id,
            JobResult {
                job_id: job.id.clone(),
                success: true,
                output: None,
                error: None,
                completed_at: chrono::Utc::now(),
            },
        )
        .unwrap();
    assert!(queue.cancel_job(&job.id).is_err());
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.stats().completed, 1);
    assert!(queue.next_job().is_none());
    assert_eq!(
        queue
            .submit(JobConfig::new("extra".into(), serde_json::json!({})))
            .unwrap_err(),
        JobError::QueueFull
    );
}

#[test]
fn job_queue_preserves_priority_fifo_and_global_capacity_between_dispatches() {
    let mut queue = JobQueue::new(6);
    for (name, priority) in [
        ("low-a", JobPriority::Low),
        ("normal", JobPriority::Normal),
        ("high-a", JobPriority::High),
        ("critical-a", JobPriority::Critical),
        ("low-b", JobPriority::Low),
        ("high-b", JobPriority::High),
    ] {
        queue
            .submit(JobConfig::new(name.into(), serde_json::json!({})).with_priority(priority))
            .unwrap();
    }
    assert_eq!(queue.pending_count(), 6);
    assert_eq!(
        queue
            .submit(JobConfig::new("extra".into(), serde_json::json!({})))
            .unwrap_err(),
        JobError::QueueFull
    );
    assert_eq!(queue.next_job().unwrap().name, "critical-a");
    queue
        .submit(
            JobConfig::new("critical-b".into(), serde_json::json!({}))
                .with_priority(JobPriority::Critical),
        )
        .unwrap();
    for expected in ["critical-b", "high-a", "high-b", "normal", "low-a", "low-b"] {
        assert_eq!(queue.next_job().unwrap().name, expected);
    }
    assert!(queue.next_job().is_none());
    assert!(queue.is_empty());
}
