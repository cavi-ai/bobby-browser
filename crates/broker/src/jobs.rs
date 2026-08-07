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

/// Sleep handler: waits `payload.ms` milliseconds (default 1000, capped at 30s).
/// Used by operator probes and e2e cancel coverage.
pub struct SleepHandler;

#[async_trait]
impl JobHandler for SleepHandler {
    async fn execute(&self, job: &Job) -> Result<serde_json::Value, String> {
        let ms = job
            .payload
            .get("ms")
            .and_then(|value| value.as_u64())
            .unwrap_or(1_000)
            .min(30_000);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Ok(serde_json::json!({ "sleptMs": ms }))
    }
}

/// HTTP readiness probe: HEAD/GET a URL under the same SSRF policy as downloads.
pub struct HttpProbeHandler;

#[async_trait]
impl JobHandler for HttpProbeHandler {
    async fn execute(&self, job: &Job) -> Result<serde_json::Value, String> {
        let url = job
            .payload
            .get("url")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "http_probe requires payload.url".to_owned())?;
        let method = job
            .payload
            .get("method")
            .and_then(|value| value.as_str())
            .map(|raw| {
                network_engine::HttpProbeMethod::parse(raw)
                    .ok_or_else(|| format!("http_probe method must be HEAD or GET, got {raw}"))
            })
            .transpose()?
            .unwrap_or(network_engine::HttpProbeMethod::Head);
        let timeout_ms = job
            .payload
            .get("timeoutMs")
            .and_then(|value| value.as_u64());
        network_engine::http_probe(
            url,
            method,
            timeout_ms,
            network_engine::NetworkPolicy::default(),
        )
        .await
    }
}

/// Poll HTTP until success or wait budget expires (same SSRF policy as `http_probe`).
pub struct HttpWaitHandler;

#[async_trait]
impl JobHandler for HttpWaitHandler {
    async fn execute(&self, job: &Job) -> Result<serde_json::Value, String> {
        let url = job
            .payload
            .get("url")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "http_wait requires payload.url".to_owned())?;
        let method = job
            .payload
            .get("method")
            .and_then(|value| value.as_str())
            .map(|raw| {
                network_engine::HttpProbeMethod::parse(raw)
                    .ok_or_else(|| format!("http_wait method must be HEAD or GET, got {raw}"))
            })
            .transpose()?
            .unwrap_or(network_engine::HttpProbeMethod::Head);
        let options = network_engine::HttpWaitOptions {
            timeout_ms: job
                .payload
                .get("timeoutMs")
                .and_then(|value| value.as_u64()),
            interval_ms: job
                .payload
                .get("intervalMs")
                .and_then(|value| value.as_u64()),
            probe_timeout_ms: job
                .payload
                .get("probeTimeoutMs")
                .and_then(|value| value.as_u64()),
            contains: job.payload.get("contains").and_then(|value| value.as_str()),
            max_body_bytes: job
                .payload
                .get("maxBodyBytes")
                .and_then(|value| value.as_u64())
                .map(|value| value as usize),
        };
        network_engine::http_wait(
            url,
            method,
            options,
            network_engine::NetworkPolicy::default(),
        )
        .await
    }
}

/// Bounded GET with truncated body (SSRF-safe). Optional `contains` substring gate.
pub struct HttpFetchHandler;

#[async_trait]
impl JobHandler for HttpFetchHandler {
    async fn execute(&self, job: &Job) -> Result<serde_json::Value, String> {
        let url = job
            .payload
            .get("url")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "http_fetch requires payload.url".to_owned())?;
        let timeout_ms = job
            .payload
            .get("timeoutMs")
            .and_then(|value| value.as_u64());
        let max_body_bytes = job
            .payload
            .get("maxBodyBytes")
            .and_then(|value| value.as_u64())
            .map(|value| value as usize);
        let contains = job.payload.get("contains").and_then(|value| value.as_str());
        network_engine::http_fetch(
            url,
            timeout_ms,
            max_body_bytes,
            contains,
            network_engine::NetworkPolicy::default(),
        )
        .await
    }
}

pub fn register_builtin_handlers(scheduler: &mut JobScheduler) {
    scheduler.register_handler("echo".to_string(), Arc::new(EchoHandler));
    scheduler.register_handler("sleep".to_string(), Arc::new(SleepHandler));
    scheduler.register_handler("http_probe".to_string(), Arc::new(HttpProbeHandler));
    scheduler.register_handler("http_wait".to_string(), Arc::new(HttpWaitHandler));
    scheduler.register_handler("http_fetch".to_string(), Arc::new(HttpFetchHandler));
}

/// Built-in handler names registered by [`register_builtin_handlers`].
pub const BUILTIN_JOB_HANDLERS: &[&str] =
    &["echo", "sleep", "http_probe", "http_wait", "http_fetch"];

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
