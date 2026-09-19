//! Built-in bounded job handlers shared by HTTP and MCP surfaces.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use types::Capability;

use crate::{Job, JobHandler, JobScheduler};

const NETWORK_EGRESS: &[Capability] = &[Capability::NetworkEgress];

pub const BUILTIN_JOB_HANDLERS: &[&str] =
    &["echo", "sleep", "http_probe", "http_wait", "http_fetch"];

pub fn register_builtin_handlers(scheduler: &mut JobScheduler) {
    scheduler.register_handler("echo".to_owned(), Arc::new(EchoHandler));
    scheduler.register_handler("sleep".to_owned(), Arc::new(SleepHandler));
    scheduler.register_handler("http_probe".to_owned(), Arc::new(HttpProbeHandler));
    scheduler.register_handler("http_wait".to_owned(), Arc::new(HttpWaitHandler));
    scheduler.register_handler("http_fetch".to_owned(), Arc::new(HttpFetchHandler));
}

struct EchoHandler;

#[async_trait]
impl JobHandler for EchoHandler {
    async fn execute(&self, job: &Job) -> Result<serde_json::Value, String> {
        Ok(job.payload.clone())
    }
}

struct SleepHandler;

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

struct HttpProbeHandler;

#[async_trait]
impl JobHandler for HttpProbeHandler {
    fn required_capabilities(&self) -> &'static [Capability] {
        NETWORK_EGRESS
    }

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

struct HttpWaitHandler;

#[async_trait]
impl JobHandler for HttpWaitHandler {
    fn required_capabilities(&self) -> &'static [Capability] {
        NETWORK_EGRESS
    }

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
                .and_then(|value| usize::try_from(value).ok()),
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

struct HttpFetchHandler;

#[async_trait]
impl JobHandler for HttpFetchHandler {
    fn required_capabilities(&self) -> &'static [Capability] {
        NETWORK_EGRESS
    }

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
            .and_then(|value| usize::try_from(value).ok());
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
