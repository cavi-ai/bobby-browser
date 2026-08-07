//! Optional job surface for MCP (`job_submit` / `job_status` / `job_cancel`).
//!
//! The HTTP API owns durable jobs via `bobby serve`. MCP tools call the same
//! scheduler through [`JobPort`] so stdio and streamable-HTTP agents share one
//! contract. Wire JSON never names `task-scheduler` types (crate_boundary).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use task_scheduler::{
    Job, JobConfig, JobError, JobHandler, JobId, JobPriority, JobScheduler, JobStatus,
};
use types::PrincipalId;

/// One `job_submit` request, less the owner.
///
/// A struct rather than a parameter list: the six fields travel together from
/// the tool call to the scheduler config, and as loose arguments they were one
/// over clippy's `too_many_arguments` bound.
pub struct JobSubmission {
    pub name: String,
    pub payload: Value,
    pub priority: JobPriorityWire,
    pub max_retries: u32,
    pub timeout_ms: Option<u64>,
    pub correlation_id: Option<String>,
}

/// Submit / read / cancel jobs for one MCP connection's principal.
#[async_trait]
pub trait JobPort: Send + Sync {
    async fn submit(
        &self,
        owner: &PrincipalId,
        request: JobSubmission,
    ) -> Result<JobSubmitWire, JobPortError>;

    async fn status(
        &self,
        owner: &PrincipalId,
        job_id: &str,
    ) -> Result<JobStatusWire, JobPortError>;

    async fn cancel(&self, owner: &PrincipalId, job_id: &str) -> Result<(), JobPortError>;
}

#[derive(Debug, Clone, Copy, Default)]
pub enum JobPriorityWire {
    Low,
    #[default]
    Normal,
    High,
    Critical,
}

impl JobPriorityWire {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "low" => Some(Self::Low),
            "normal" => Some(Self::Normal),
            "high" => Some(Self::High),
            "critical" => Some(Self::Critical),
            _ => None,
        }
    }

    fn into_scheduler(self) -> JobPriority {
        match self {
            Self::Low => JobPriority::Low,
            Self::Normal => JobPriority::Normal,
            Self::High => JobPriority::High,
            Self::Critical => JobPriority::Critical,
        }
    }
}

#[derive(Debug, Clone)]
pub struct JobSubmitWire {
    pub job_id: String,
    pub status: String,
}

impl JobSubmitWire {
    pub fn to_value(&self) -> Value {
        json!({
            "jobId": self.job_id,
            "status": self.status,
        })
    }
}

#[derive(Debug, Clone)]
pub struct JobStatusWire {
    pub id: String,
    pub name: String,
    pub priority: String,
    pub status: String,
    pub payload: Value,
    pub created_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub retry_count: u32,
    pub max_retries: u32,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub timeout_ms: Option<u64>,
    pub correlation_id: Option<String>,
}

impl JobStatusWire {
    pub fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "priority": self.priority,
            "status": self.status,
            "payload": self.payload,
            "createdAt": self.created_at,
            "startedAt": self.started_at,
            "completedAt": self.completed_at,
            "retryCount": self.retry_count,
            "maxRetries": self.max_retries,
            "result": self.result,
            "error": self.error,
            "timeoutMs": self.timeout_ms,
            "correlationId": self.correlation_id,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobPortError {
    NotFound,
    InvalidName,
    InvalidPriority,
    Unavailable(String),
}

impl JobPortError {
    pub fn message(&self) -> String {
        match self {
            Self::NotFound => "job not found".to_owned(),
            Self::InvalidName => "job name must be nonempty".to_owned(),
            Self::InvalidPriority => {
                "priority must be one of low, normal, high, critical".to_owned()
            }
            Self::Unavailable(detail) => detail.clone(),
        }
    }
}

/// In-process scheduler used by `bobby mcp-stdio` and `bobby serve` MCP HTTP.
pub struct InProcessJobPort {
    scheduler: Arc<JobScheduler>,
}

impl InProcessJobPort {
    pub fn new(scheduler: Arc<JobScheduler>) -> Self {
        Self { scheduler }
    }

    /// Memory scheduler with built-in handlers; caller must `spawn` `run()`.
    pub fn memory() -> (Self, Arc<JobScheduler>) {
        let mut scheduler = JobScheduler::new(task_scheduler::SchedulerConfig::default());
        register_builtin_handlers(&mut scheduler);
        let scheduler = Arc::new(scheduler);
        (Self::new(Arc::clone(&scheduler)), scheduler)
    }

    pub fn from_scheduler(mut scheduler: JobScheduler) -> (Self, Arc<JobScheduler>) {
        register_builtin_handlers(&mut scheduler);
        let scheduler = Arc::new(scheduler);
        (Self::new(Arc::clone(&scheduler)), scheduler)
    }
}

fn register_builtin_handlers(scheduler: &mut JobScheduler) {
    scheduler.register_handler("echo".to_string(), Arc::new(EchoHandler));
    scheduler.register_handler("sleep".to_string(), Arc::new(SleepHandler));
    scheduler.register_handler("http_probe".to_string(), Arc::new(HttpProbeHandler));
    scheduler.register_handler("http_wait".to_string(), Arc::new(HttpWaitHandler));
}

struct EchoHandler;

#[async_trait]
impl JobHandler for EchoHandler {
    async fn execute(&self, job: &Job) -> Result<Value, String> {
        Ok(job.payload.clone())
    }
}

struct SleepHandler;

#[async_trait]
impl JobHandler for SleepHandler {
    async fn execute(&self, job: &Job) -> Result<Value, String> {
        let ms = job
            .payload
            .get("ms")
            .and_then(|value| value.as_u64())
            .unwrap_or(1_000)
            .min(30_000);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Ok(json!({ "sleptMs": ms }))
    }
}

struct HttpProbeHandler;

#[async_trait]
impl JobHandler for HttpProbeHandler {
    async fn execute(&self, job: &Job) -> Result<Value, String> {
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
    async fn execute(&self, job: &Job) -> Result<Value, String> {
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
        let timeout_ms = job
            .payload
            .get("timeoutMs")
            .and_then(|value| value.as_u64());
        let interval_ms = job
            .payload
            .get("intervalMs")
            .and_then(|value| value.as_u64());
        let probe_timeout_ms = job
            .payload
            .get("probeTimeoutMs")
            .and_then(|value| value.as_u64());
        network_engine::http_wait(
            url,
            method,
            timeout_ms,
            interval_ms,
            probe_timeout_ms,
            network_engine::NetworkPolicy::default(),
        )
        .await
    }
}

fn status_wire(status: &JobStatus) -> String {
    match status {
        JobStatus::Pending => "pending",
        JobStatus::Running => "running",
        JobStatus::Completed => "completed",
        JobStatus::Failed => "failed",
        JobStatus::Cancelled => "cancelled",
    }
    .to_owned()
}

fn priority_wire(priority: &JobPriority) -> String {
    match priority {
        JobPriority::Low => "low",
        JobPriority::Normal => "normal",
        JobPriority::High => "high",
        JobPriority::Critical => "critical",
    }
    .to_owned()
}

fn job_to_wire(job: Job) -> JobStatusWire {
    JobStatusWire {
        id: job.id.0,
        name: job.name,
        priority: priority_wire(&job.priority),
        status: status_wire(&job.status),
        payload: job.payload,
        created_at: job.created_at.to_rfc3339(),
        started_at: job.started_at.map(|t| t.to_rfc3339()),
        completed_at: job.completed_at.map(|t| t.to_rfc3339()),
        retry_count: job.retry_count,
        max_retries: job.max_retries,
        result: job.result.map(|r| {
            json!({
                "jobId": r.job_id.0,
                "success": r.success,
                "output": r.output,
                "error": r.error,
                "completedAt": r.completed_at.to_rfc3339(),
            })
        }),
        error: job.error,
        timeout_ms: job.timeout_ms,
        correlation_id: job.correlation_id,
    }
}

fn map_scheduler_error(error: JobError) -> JobPortError {
    match error {
        JobError::NotFound(_) => JobPortError::NotFound,
        other => JobPortError::Unavailable(other.to_string()),
    }
}

#[async_trait]
impl JobPort for InProcessJobPort {
    async fn submit(
        &self,
        owner: &PrincipalId,
        request: JobSubmission,
    ) -> Result<JobSubmitWire, JobPortError> {
        let JobSubmission {
            name,
            payload,
            priority,
            max_retries,
            timeout_ms,
            correlation_id,
        } = request;
        if name.trim().is_empty() {
            return Err(JobPortError::InvalidName);
        }
        let mut config = JobConfig::new(name, payload)
            .with_priority(priority.into_scheduler())
            .with_max_retries(max_retries)
            .with_owner(owner.clone());
        if let Some(timeout_ms) = timeout_ms {
            config = config.with_timeout(Duration::from_millis(timeout_ms));
        }
        if let Some(correlation_id) = correlation_id {
            config = config.with_correlation_id(correlation_id);
        }
        let id = self
            .scheduler
            .submit(config)
            .await
            .map_err(map_scheduler_error)?;
        let status = self
            .scheduler
            .get_job(&id)
            .await
            .map(|job| status_wire(&job.status))
            .unwrap_or_else(|| "pending".to_owned());
        Ok(JobSubmitWire {
            job_id: id.0,
            status,
        })
    }

    async fn status(
        &self,
        owner: &PrincipalId,
        job_id: &str,
    ) -> Result<JobStatusWire, JobPortError> {
        let id = JobId(job_id.to_owned());
        let Some(job) = self.scheduler.get_job(&id).await else {
            return Err(JobPortError::NotFound);
        };
        if job
            .owner
            .as_ref()
            .is_some_and(|job_owner| job_owner != owner)
        {
            return Err(JobPortError::NotFound);
        }
        Ok(job_to_wire(job))
    }

    async fn cancel(&self, owner: &PrincipalId, job_id: &str) -> Result<(), JobPortError> {
        let id = JobId(job_id.to_owned());
        match self.scheduler.get_job(&id).await {
            Some(job)
                if job
                    .owner
                    .as_ref()
                    .is_some_and(|job_owner| job_owner != owner) =>
            {
                return Err(JobPortError::NotFound);
            }
            _ => {}
        }
        self.scheduler
            .cancel_job(&id)
            .await
            .map_err(map_scheduler_error)
    }
}

/// True when `name` is a job tool (advertised only with a [`JobPort`]).
pub fn is_job_tool(name: &str) -> bool {
    matches!(name, "job_submit" | "job_status" | "job_cancel")
}
