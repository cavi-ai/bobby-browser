//! Job request and response contracts for `/v1/jobs`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::JobId;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum JobPriority {
    Low,
    #[default]
    Normal,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubmitJobRequest {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<JobPriority>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl SubmitJobRequest {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.name.trim().is_empty() {
            return Err("job name must not be empty");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobSubmitResponse {
    pub job_id: JobId,
    pub status: JobStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobResult {
    pub job_id: JobId,
    pub success: bool,
    pub output: Value,
    pub error: Option<String>,
    pub completed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobStatusResponse {
    pub id: JobId,
    pub name: String,
    pub priority: JobPriority,
    pub status: JobStatus,
    pub payload: Value,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub retry_count: u32,
    pub max_retries: u32,
    pub result: Option<JobResult>,
    pub error: Option<String>,
    pub timeout_ms: Option<u64>,
    pub correlation_id: Option<Uuid>,
}

impl JobStatusResponse {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.name.trim().is_empty() || self.retry_count > self.max_retries {
            return Err("job status response violates bounded fields");
        }
        if self
            .result
            .as_ref()
            .is_some_and(|result| result.job_id != self.id)
        {
            return Err("job result identifier does not match its status");
        }

        let valid = match self.status {
            JobStatus::Pending => {
                self.started_at.is_none()
                    && self.completed_at.is_none()
                    && self.result.is_none()
                    && self.error.is_none()
            }
            JobStatus::Running => {
                self.started_at.is_some()
                    && self.completed_at.is_none()
                    && self.result.is_none()
                    && self.error.is_none()
            }
            JobStatus::Completed => {
                self.started_at.is_some()
                    && self.completed_at.is_some()
                    && self
                        .result
                        .as_ref()
                        .is_some_and(|result| result.success && result.error.is_none())
                    && self.error.is_none()
            }
            JobStatus::Failed => {
                self.started_at.is_some()
                    && self.completed_at.is_some()
                    && self.result.is_none()
                    && self.error.as_ref().is_some_and(|error| !error.is_empty())
            }
            JobStatus::Cancelled => {
                self.completed_at.is_some() && self.result.is_none() && self.error.is_none()
            }
        };
        valid
            .then_some(())
            .ok_or("job status response violates lifecycle invariants")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn job_identifiers_require_the_runtime_prefix_and_uuid() {
        assert!(serde_json::from_value::<JobId>(json!(format!("job_{}", Uuid::new_v4()))).is_ok());
        assert!(serde_json::from_value::<JobId>(json!(Uuid::new_v4().to_string())).is_err());
        assert!(serde_json::from_value::<JobId>(json!("job_not-a-uuid")).is_err());
    }

    #[test]
    fn job_status_validation_enforces_lifecycle_invariants() {
        let id = JobId::new();
        let mut response: JobStatusResponse = serde_json::from_value(json!({
            "id": id,
            "name": "fetch-report",
            "priority": "normal",
            "status": "running",
            "payload": null,
            "createdAt": "2026-09-19T00:00:00Z",
            "startedAt": "2026-09-19T00:00:01Z",
            "completedAt": null,
            "retryCount": 0,
            "maxRetries": 3,
            "result": null,
            "error": null,
            "timeoutMs": null,
            "correlationId": null,
        }))
        .unwrap();
        assert!(response.validate().is_ok());
        response.started_at = None;
        assert!(response.validate().is_err());
    }
}
