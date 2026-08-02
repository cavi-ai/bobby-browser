//! Built-in job handlers and scheduler bootstrap helpers for the broker.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Duration as ChronoDuration;
use config::AppConfig;
use interface_core::{IdempotencyStore, RetainedOutcome};
use serde::{Deserialize, Serialize};
use task_scheduler::{Job, JobHandler, JobId, JobScheduler, JobStatus, SchedulerConfig};
use tracing::info;

/// Retained submit response for idempotent `POST /v1/jobs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSubmitOutcome {
    pub job_id: JobId,
    pub status: JobStatus,
}

impl RetainedOutcome for JobSubmitOutcome {
    fn releases_reservation(&self) -> bool {
        false
    }

    fn safety_relevant(&self) -> bool {
        false
    }
}

/// Echo handler: returns the job payload as the result.
pub struct EchoHandler;

#[async_trait]
impl JobHandler for EchoHandler {
    async fn execute(&self, job: &Job) -> Result<serde_json::Value, String> {
        Ok(job.payload.clone())
    }
}

pub fn register_builtin_handlers(scheduler: &mut JobScheduler) {
    scheduler.register_handler("echo".to_string(), Arc::new(EchoHandler));
}

/// Build scheduler config from app storage + server drain settings.
pub fn scheduler_config_from_app(config: &AppConfig) -> SchedulerConfig {
    SchedulerConfig::default()
        .with_journal_path(config.storage.scheduler_journal_path.clone())
        .with_drain_timeout(config.server.shutdown_timeout_ms)
}

/// In-memory scheduler for tests and lightweight routers.
pub fn memory_scheduler() -> JobScheduler {
    let mut scheduler = JobScheduler::new(SchedulerConfig::default().with_drain_timeout(5_000));
    register_builtin_handlers(&mut scheduler);
    scheduler
}

/// Journal-backed scheduler from app config.
pub async fn journal_scheduler(
    config: &AppConfig,
) -> Result<JobScheduler, task_scheduler::JobError> {
    let mut scheduler = JobScheduler::from_config(scheduler_config_from_app(config)).await?;
    register_builtin_handlers(&mut scheduler);
    Ok(scheduler)
}

pub fn job_idempotency_store() -> IdempotencyStore<JobSubmitOutcome> {
    IdempotencyStore::with_global_capacity(256, 4096, ChronoDuration::minutes(15))
}

pub async fn shutdown_scheduler(
    scheduler: &JobScheduler,
    run_handle: tokio::task::JoinHandle<Result<(), task_scheduler::JobError>>,
    drain: Duration,
) {
    scheduler.request_shutdown();
    info!("scheduler.shutdown");
    match tokio::time::timeout(drain, run_handle).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(task_scheduler::JobError::DrainTimeout))) => {
            tracing::warn!("scheduler drain timed out");
        }
        Ok(Ok(Err(e))) => tracing::warn!(error = %e, "scheduler run ended with error"),
        Ok(Err(e)) => tracing::warn!(error = %e, "scheduler task join error"),
        Err(_) => tracing::warn!("scheduler run join exceeded drain timeout"),
    }
}
