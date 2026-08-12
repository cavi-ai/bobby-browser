//! Session and page runtime state returned by `/v1` endpoints.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{ExecutionPolicy, PageId, SessionId};

/// Page rendering / interaction mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum PageMode {
    Document,
    Interactive,
    Render,
}

/// `GET /v1/runtime` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RuntimeInfo {
    pub version: String,
    pub capabilities: Vec<String>,
    pub active_sessions: usize,
    pub queued_jobs: usize,
    pub uptime_ms: u64,
    #[serde(
        default,
        rename = "operationalMetrics",
        skip_serializing_if = "Option::is_none"
    )]
    pub operational_metrics: Option<OperationalMetricsSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct OperationalMetricsSnapshot {
    pub observation_window_ms: u64,
    pub intent: IntentMetricsSnapshot,
    pub context: ContextMetricsSnapshot,
    pub prefill: PrefillMetricsSnapshot,
    pub vision: VisionMetricsSnapshot,
    pub verification: VerificationMetricsSnapshot,
    pub retries: RetryMetricsSnapshot,
    pub reconciliation: ReconciliationMetricsSnapshot,
    pub workflow_calls: WorkflowCallMetricsSnapshot,
}

macro_rules! metric_snapshot {
    ($name:ident { $($field:ident),+ $(,)? }) => {
        #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
        #[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
        #[serde(rename_all = "camelCase")]
        pub struct $name { $(pub $field: u64,)+ }
    };
}

metric_snapshot!(IntentMetricsSnapshot {
    total,
    locate,
    fill,
    complete_form,
    extract,
    submit,
    wait_for_state,
    follow,
    dismiss,
    deterministic,
    context,
    vision_prefill,
    vision_fallback,
});
metric_snapshot!(ContextMetricsSnapshot {
    hit,
    miss,
    ambiguous_refusal,
    stale_rejection,
    error,
});
metric_snapshot!(PrefillMetricsSnapshot {
    hit,
    miss,
    dropped_entry,
    policy_denied,
    provider_failure,
});
metric_snapshot!(ConfidenceMetricsSnapshot {
    below_acceptance,
    accepted,
    high,
    unreported,
});
metric_snapshot!(VerificationMetricsSnapshot {
    accepted,
    target_not_found,
    target_ambiguous,
    obstruction_persisted,
    value_mismatch,
    other_rejected,
});
metric_snapshot!(RetryMetricsSnapshot {
    transport,
    timeout,
    target_detached,
    state_conflict,
    other,
});
metric_snapshot!(ReconciliationMetricsSnapshot {
    resumed,
    restarted,
    needs_reconciliation,
    failed,
});
metric_snapshot!(WorkflowCallMetricsSnapshot {
    lifecycle,
    discovery,
    read,
    mutation,
    composite_workflow,
    recovery,
    artifact,
    job,
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct VisionMetricsSnapshot {
    pub attempted: u64,
    pub accepted: u64,
    pub rejected: u64,
    pub abstained: u64,
    pub timed_out: u64,
    pub failed: u64,
    pub provider_http: u64,
    pub provider_acp: u64,
    pub provider_direct_local: u64,
    pub latency_ms: LatencyHistogramSnapshot,
    pub confidence: ConfidenceMetricsSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct LatencyHistogramSnapshot {
    pub buckets: Vec<LatencyBucketSnapshot>,
    pub overflow: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct LatencyBucketSnapshot {
    pub upper_bound_ms: u64,
    pub count: u64,
}

/// Browser session returned by session create/list endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionState {
    pub id: SessionId,
    pub profile: String,
    pub proxy: Option<String>,
    pub page_ids: Vec<PageId>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
    pub execution_policy: ExecutionPolicy,
}

/// Page within a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PageState {
    pub id: PageId,
    pub session_id: SessionId,
    pub url: Option<String>,
    pub mode: PageMode,
    pub ready_state: String,
    pub pending_requests: usize,
}

/// Result of a navigation command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NavigationResult {
    pub page_id: PageId,
    pub url: String,
    pub ready_state: String,
}

/// Result of a structured extract command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ExtractResult {
    pub page_id: PageId,
    pub data: serde_json::Value,
}
