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
    /// Operator-set health budget for one vision propose round-trip
    /// (`[vision].propose_budget_ms`); absent when no budget is configured.
    #[serde(
        default,
        rename = "visionProposeBudgetMs",
        skip_serializing_if = "Option::is_none"
    )]
    pub vision_propose_budget_ms: Option<u64>,
    #[serde(
        default,
        rename = "operationalMetrics",
        skip_serializing_if = "Option::is_none"
    )]
    pub operational_metrics: Option<OperationalMetricsSnapshot>,
    /// Per-provider health derived from recorded vision proposal outcomes and
    /// the configured budgets (`[vision].propose_budget_ms`,
    /// `[vision].health_failure_threshold`). Report-only. Absent when no
    /// vision provider is configured.
    #[serde(
        default,
        rename = "providerHealth",
        skip_serializing_if = "Option::is_none"
    )]
    pub provider_health: Option<Vec<ProviderHealthSnapshot>>,
}

/// Health classification of one vision provider boundary. `degraded` means
/// consecutive propose-budget violations reached the failure threshold;
/// `unhealthy` means consecutive provider failures did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum ProviderHealthStatus {
    Healthy,
    Degraded,
    Unhealthy,
}

/// Operator-facing health of one vision provider mode. Carries the thresholds
/// that produced `status` so consumers never re-derive them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ProviderHealthSnapshot {
    pub provider_mode: String,
    pub status: ProviderHealthStatus,
    pub successes: u64,
    pub failures: u64,
    pub consecutive_failures: u64,
    pub budget_violations: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_budget_ms: Option<u64>,
    pub failure_threshold: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct OperationalMetricsSnapshot {
    pub observation_window_ms: u64,
    pub intent: IntentMetricsSnapshot,
    pub context: ContextMetricsSnapshot,
    #[serde(default)]
    pub context_ranked_vision: ContextRankedVisionMetricsSnapshot,
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
    solve_challenge,
    detect_challenge,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ContextRankedVisionMetricsSnapshot {
    pub attempted: u64,
    pub source_observed: u64,
    pub source_vision_promoted: u64,
    pub source_unreported: u64,
    pub hit: u64,
    pub miss: u64,
    pub ambiguous_refusal: u64,
    pub stale_rejection: u64,
    pub error: u64,
    pub provider_escalations: u64,
    pub provider_http: u64,
    pub provider_acp: u64,
    pub provider_direct_local: u64,
    pub candidate_ranking_latency_ms: LatencyHistogramSnapshot,
    pub confidence: ConfidenceMetricsSnapshot,
    pub verification_accepted: u64,
    pub verification_rejected: u64,
}

impl Default for ContextRankedVisionMetricsSnapshot {
    fn default() -> Self {
        Self {
            attempted: 0,
            source_observed: 0,
            source_vision_promoted: 0,
            source_unreported: 0,
            hit: 0,
            miss: 0,
            ambiguous_refusal: 0,
            stale_rejection: 0,
            error: 0,
            provider_escalations: 0,
            provider_http: 0,
            provider_acp: 0,
            provider_direct_local: 0,
            candidate_ranking_latency_ms: LatencyHistogramSnapshot {
                buckets: Vec::new(),
                overflow: 0,
            },
            confidence: ConfidenceMetricsSnapshot {
                below_acceptance: 0,
                accepted: 0,
                high: 0,
                unreported: 0,
            },
            verification_accepted: 0,
            verification_rejected: 0,
        }
    }
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
    /// Set at creation by `CreateSessionRequest::zigzagzig`: page-bound
    /// commands run under the ZigZagZig recovery ladder. Recorded
    /// separately from the policy — a hand-assembled everything-on policy
    /// does not opt into the ladder. Absent from the wire when off, so
    /// plain sessions carry no godmode baggage.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub zigzagzig: bool,
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
