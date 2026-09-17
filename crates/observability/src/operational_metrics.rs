use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::provider_health::{ProviderCallOutcome, ProviderHealthTracker};

pub use types::{
    ConfidenceMetricsSnapshot, ContextMetricsSnapshot, ContextRankedVisionMetricsSnapshot,
    IntentMetricsSnapshot, LatencyBucketSnapshot, LatencyHistogramSnapshot,
    OperationalMetricsSnapshot, PrefillMetricsSnapshot, ProviderHealthSnapshot,
    ReconciliationMetricsSnapshot, RetryMetricsSnapshot, VerificationMetricsSnapshot,
    VisionMetricsSnapshot, WorkflowCallMetricsSnapshot,
};

const LATENCY_UPPER_BOUNDS_MS: [u64; 10] =
    [25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 15_000];
const CONFIDENCE_ACCEPTANCE_THRESHOLD: f64 = 0.75;
const CONFIDENCE_HIGH_THRESHOLD: f64 = 0.90;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentMetricKind {
    Locate,
    Fill,
    CompleteForm,
    Extract,
    Submit,
    WaitForState,
    Follow,
    Dismiss,
    SolveChallenge,
    DetectChallenge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionSource {
    Deterministic,
    Context,
    VisionPrefill,
    VisionFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextLookupOutcome {
    Hit,
    Miss,
    AmbiguousRefusal,
    StaleRejection,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuralContextSource {
    Observed,
    VisionPromoted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextCandidateRankingMetric {
    pub source: Option<StructuralContextSource>,
    pub outcome: ContextLookupOutcome,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextRankedVisionMetric {
    pub provider_mode: ProviderMode,
    pub confidence: Option<f64>,
    pub verification: Option<VerificationMetricResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefillOutcome {
    Hit,
    Miss,
    DroppedEntry,
    PolicyDenied,
    ProviderFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderMode {
    Http,
    Acp,
    DirectLocal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisionProposalOutcome {
    Accepted,
    Rejected,
    Abstained,
    TimedOut,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VisionProposalMetric {
    pub provider_mode: ProviderMode,
    pub latency_ms: u64,
    pub confidence: Option<f64>,
    pub outcome: VisionProposalOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationMetricResult {
    Accepted,
    TargetNotFound,
    TargetAmbiguous,
    ObstructionPersisted,
    ValueMismatch,
    OtherRejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryClass {
    Transport,
    Timeout,
    TargetDetached,
    StateConflict,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconciliationMetricOutcome {
    Resumed,
    Restarted,
    NeedsReconciliation,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowCallClass {
    Lifecycle,
    Discovery,
    Read,
    Mutation,
    CompositeWorkflow,
    Recovery,
    Artifact,
    Job,
}

#[derive(Clone)]
pub struct OperationalMetrics {
    inner: Arc<OperationalMetricsInner>,
}

struct OperationalMetricsInner {
    started_at: Instant,
    provider_health: ProviderHealthTracker,
    intent_kind: [AtomicU64; 10],
    resolution_source: [AtomicU64; 4],
    context: [AtomicU64; 5],
    context_rank_source: [AtomicU64; 3],
    context_rank_outcome: [AtomicU64; 5],
    context_rank_latency: [AtomicU64; 11],
    context_rank_provider: [AtomicU64; 3],
    context_rank_confidence: [AtomicU64; 4],
    context_rank_verification: [AtomicU64; 2],
    prefill: [AtomicU64; 5],
    vision_outcome: [AtomicU64; 5],
    provider_mode: [AtomicU64; 3],
    latency: [AtomicU64; 11],
    confidence: [AtomicU64; 4],
    verification: [AtomicU64; 6],
    retries: [AtomicU64; 5],
    reconciliation: [AtomicU64; 4],
    workflow_calls: [AtomicU64; 8],
}

impl Default for OperationalMetrics {
    fn default() -> Self {
        Self::with_health_budget(None, 3)
    }
}

impl OperationalMetrics {
    /// Metrics with provider-health enforcement thresholds. `latency_budget_ms`
    /// is `[vision].propose_budget_ms`; `failure_threshold` is
    /// `[vision].health_failure_threshold`.
    pub fn with_health_budget(latency_budget_ms: Option<u64>, failure_threshold: u32) -> Self {
        Self {
            inner: Arc::new(OperationalMetricsInner {
                started_at: Instant::now(),
                provider_health: ProviderHealthTracker::new(latency_budget_ms, failure_threshold),
                intent_kind: atomic_array(),
                resolution_source: atomic_array(),
                context: atomic_array(),
                context_rank_source: atomic_array(),
                context_rank_outcome: atomic_array(),
                context_rank_latency: atomic_array(),
                context_rank_provider: atomic_array(),
                context_rank_confidence: atomic_array(),
                context_rank_verification: atomic_array(),
                prefill: atomic_array(),
                vision_outcome: atomic_array(),
                provider_mode: atomic_array(),
                latency: atomic_array(),
                confidence: atomic_array(),
                verification: atomic_array(),
                retries: atomic_array(),
                reconciliation: atomic_array(),
                workflow_calls: atomic_array(),
            }),
        }
    }

    /// Per-provider health derived from recorded vision proposals and the
    /// configured budgets. Report-only; empty until a provider serves a call.
    pub fn provider_health(&self) -> Vec<ProviderHealthSnapshot> {
        self.inner.provider_health.snapshot()
    }

    pub fn record_intent_resolution(&self, kind: IntentMetricKind, source: ResolutionSource) {
        increment(&self.inner.intent_kind[kind as usize]);
        increment(&self.inner.resolution_source[source as usize]);
    }

    pub fn record_context_lookup(&self, outcome: ContextLookupOutcome) {
        increment(&self.inner.context[outcome as usize]);
    }

    pub fn record_context_candidate_ranking(&self, metric: ContextCandidateRankingMetric) {
        let source_index = match metric.source {
            Some(StructuralContextSource::Observed) => 0,
            Some(StructuralContextSource::VisionPromoted) => 1,
            None => 2,
        };
        increment(&self.inner.context_rank_source[source_index]);
        increment(&self.inner.context_rank_outcome[metric.outcome as usize]);
        increment_latency(&self.inner.context_rank_latency, metric.latency_ms);
    }

    pub fn record_context_ranked_vision(&self, metric: ContextRankedVisionMetric) {
        increment(&self.inner.context_rank_provider[metric.provider_mode as usize]);
        increment_confidence(&self.inner.context_rank_confidence, metric.confidence);
        if let Some(result) = metric.verification {
            let index = usize::from(result != VerificationMetricResult::Accepted);
            increment(&self.inner.context_rank_verification[index]);
        }
    }

    pub fn record_prefill(&self, outcome: PrefillOutcome) {
        increment(&self.inner.prefill[outcome as usize]);
    }

    pub fn record_vision_proposal(&self, observation: VisionProposalMetric) {
        increment(&self.inner.vision_outcome[observation.outcome as usize]);
        increment(&self.inner.provider_mode[observation.provider_mode as usize]);
        increment_latency(&self.inner.latency, observation.latency_ms);
        increment_confidence(&self.inner.confidence, observation.confidence);
        let call_outcome = match observation.outcome {
            VisionProposalOutcome::Failed | VisionProposalOutcome::TimedOut => {
                ProviderCallOutcome::Failure
            }
            // Accepted, rejected, and abstained proposals all mean the
            // provider answered; the verdict belongs to verification metrics.
            VisionProposalOutcome::Accepted
            | VisionProposalOutcome::Rejected
            | VisionProposalOutcome::Abstained => ProviderCallOutcome::Success,
        };
        self.inner.provider_health.record(
            observation.provider_mode,
            call_outcome,
            observation.latency_ms,
        );
    }

    pub fn record_verification(&self, result: VerificationMetricResult) {
        increment(&self.inner.verification[result as usize]);
    }

    pub fn record_retry(&self, class: RetryClass) {
        increment(&self.inner.retries[class as usize]);
    }

    pub fn record_reconciliation(&self, outcome: ReconciliationMetricOutcome) {
        increment(&self.inner.reconciliation[outcome as usize]);
    }

    pub fn record_workflow_call(&self, class: WorkflowCallClass) {
        increment(&self.inner.workflow_calls[class as usize]);
    }

    pub fn snapshot(&self) -> OperationalMetricsSnapshot {
        let intent_kind = load(&self.inner.intent_kind);
        let resolution_source = load(&self.inner.resolution_source);
        let context = load(&self.inner.context);
        let context_rank_source = load(&self.inner.context_rank_source);
        let context_rank_outcome = load(&self.inner.context_rank_outcome);
        let context_rank_latency = load(&self.inner.context_rank_latency);
        let context_rank_provider = load(&self.inner.context_rank_provider);
        let context_rank_confidence = load(&self.inner.context_rank_confidence);
        let context_rank_verification = load(&self.inner.context_rank_verification);
        let prefill = load(&self.inner.prefill);
        let vision_outcome = load(&self.inner.vision_outcome);
        let provider_mode = load(&self.inner.provider_mode);
        let latency = load(&self.inner.latency);
        let confidence = load(&self.inner.confidence);
        let verification = load(&self.inner.verification);
        let retries = load(&self.inner.retries);
        let reconciliation = load(&self.inner.reconciliation);
        let workflow_calls = load(&self.inner.workflow_calls);

        OperationalMetricsSnapshot {
            observation_window_ms: self.inner.started_at.elapsed().as_millis() as u64,
            intent: IntentMetricsSnapshot {
                total: saturating_sum(&intent_kind),
                locate: intent_kind[0],
                fill: intent_kind[1],
                complete_form: intent_kind[2],
                extract: intent_kind[3],
                submit: intent_kind[4],
                wait_for_state: intent_kind[5],
                follow: intent_kind[6],
                dismiss: intent_kind[7],
                solve_challenge: intent_kind[8],
                detect_challenge: intent_kind[9],
                deterministic: resolution_source[0],
                context: resolution_source[1],
                vision_prefill: resolution_source[2],
                vision_fallback: resolution_source[3],
            },
            context: ContextMetricsSnapshot {
                hit: context[0],
                miss: context[1],
                ambiguous_refusal: context[2],
                stale_rejection: context[3],
                error: context[4],
            },
            context_ranked_vision: ContextRankedVisionMetricsSnapshot {
                attempted: saturating_sum(&context_rank_outcome),
                source_observed: context_rank_source[0],
                source_vision_promoted: context_rank_source[1],
                source_unreported: context_rank_source[2],
                hit: context_rank_outcome[0],
                miss: context_rank_outcome[1],
                ambiguous_refusal: context_rank_outcome[2],
                stale_rejection: context_rank_outcome[3],
                error: context_rank_outcome[4],
                provider_escalations: saturating_sum(&context_rank_provider),
                provider_http: context_rank_provider[0],
                provider_acp: context_rank_provider[1],
                provider_direct_local: context_rank_provider[2],
                candidate_ranking_latency_ms: latency_histogram(&context_rank_latency),
                confidence: confidence_snapshot(&context_rank_confidence),
                verification_accepted: context_rank_verification[0],
                verification_rejected: context_rank_verification[1],
            },
            prefill: PrefillMetricsSnapshot {
                hit: prefill[0],
                miss: prefill[1],
                dropped_entry: prefill[2],
                policy_denied: prefill[3],
                provider_failure: prefill[4],
            },
            vision: VisionMetricsSnapshot {
                attempted: saturating_sum(&vision_outcome),
                accepted: vision_outcome[0],
                rejected: vision_outcome[1],
                abstained: vision_outcome[2],
                timed_out: vision_outcome[3],
                failed: vision_outcome[4],
                provider_http: provider_mode[0],
                provider_acp: provider_mode[1],
                provider_direct_local: provider_mode[2],
                latency_ms: latency_histogram(&latency),
                confidence: confidence_snapshot(&confidence),
            },
            verification: VerificationMetricsSnapshot {
                accepted: verification[0],
                target_not_found: verification[1],
                target_ambiguous: verification[2],
                obstruction_persisted: verification[3],
                value_mismatch: verification[4],
                other_rejected: verification[5],
            },
            retries: RetryMetricsSnapshot {
                transport: retries[0],
                timeout: retries[1],
                target_detached: retries[2],
                state_conflict: retries[3],
                other: retries[4],
            },
            reconciliation: ReconciliationMetricsSnapshot {
                resumed: reconciliation[0],
                restarted: reconciliation[1],
                needs_reconciliation: reconciliation[2],
                failed: reconciliation[3],
            },
            workflow_calls: WorkflowCallMetricsSnapshot {
                lifecycle: workflow_calls[0],
                discovery: workflow_calls[1],
                read: workflow_calls[2],
                mutation: workflow_calls[3],
                composite_workflow: workflow_calls[4],
                recovery: workflow_calls[5],
                artifact: workflow_calls[6],
                job: workflow_calls[7],
            },
        }
    }
}

fn atomic_array<const N: usize>() -> [AtomicU64; N] {
    std::array::from_fn(|_| AtomicU64::new(0))
}

fn increment(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
        Some(value.saturating_add(1))
    });
}

fn increment_latency(counters: &[AtomicU64; 11], latency_ms: u64) {
    let index = LATENCY_UPPER_BOUNDS_MS
        .iter()
        .position(|bound| latency_ms <= *bound)
        .unwrap_or(LATENCY_UPPER_BOUNDS_MS.len());
    increment(&counters[index]);
}

fn increment_confidence(counters: &[AtomicU64; 4], confidence: Option<f64>) {
    let index = match confidence {
        None => 3,
        Some(value) if value < CONFIDENCE_ACCEPTANCE_THRESHOLD => 0,
        Some(value) if value < CONFIDENCE_HIGH_THRESHOLD => 1,
        Some(_) => 2,
    };
    increment(&counters[index]);
}

fn latency_histogram(counters: &[u64; 11]) -> LatencyHistogramSnapshot {
    LatencyHistogramSnapshot {
        buckets: LATENCY_UPPER_BOUNDS_MS
            .iter()
            .zip(counters.iter())
            .map(|(upper_bound_ms, count)| LatencyBucketSnapshot {
                upper_bound_ms: *upper_bound_ms,
                count: *count,
            })
            .collect(),
        overflow: counters[10],
    }
}

fn confidence_snapshot(counters: &[u64; 4]) -> ConfidenceMetricsSnapshot {
    ConfidenceMetricsSnapshot {
        below_acceptance: counters[0],
        accepted: counters[1],
        high: counters[2],
        unreported: counters[3],
    }
}

fn load<const N: usize>(counters: &[AtomicU64; N]) -> [u64; N] {
    std::array::from_fn(|index| counters[index].load(Ordering::Acquire))
}

fn saturating_sum(values: &[u64]) -> u64 {
    values
        .iter()
        .fold(0_u64, |total, value| total.saturating_add(*value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn increment_saturates() {
        let counter = AtomicU64::new(u64::MAX);
        increment(&counter);
        assert_eq!(counter.load(Ordering::Acquire), u64::MAX);
    }

    #[test]
    fn vision_proposals_drive_provider_health() {
        let metrics = OperationalMetrics::with_health_budget(Some(500), 2);
        let proposal = |outcome, latency_ms| VisionProposalMetric {
            provider_mode: ProviderMode::Http,
            latency_ms,
            confidence: None,
            outcome,
        };
        // A below-floor rejection is a verdict, not a provider failure.
        metrics.record_vision_proposal(proposal(VisionProposalOutcome::Rejected, 100));
        assert!(metrics.provider_health()[0].status == types::ProviderHealthStatus::Healthy);
        metrics.record_vision_proposal(proposal(VisionProposalOutcome::Failed, 100));
        metrics.record_vision_proposal(proposal(VisionProposalOutcome::TimedOut, 100));
        let health = metrics.provider_health();
        assert_eq!(health[0].status, types::ProviderHealthStatus::Unhealthy);
        assert_eq!(health[0].failures, 2);
        assert_eq!(health[0].successes, 1);
    }

    #[test]
    fn vision_proposals_enforce_the_latency_budget() {
        let metrics = OperationalMetrics::with_health_budget(Some(500), 2);
        let slow = VisionProposalMetric {
            provider_mode: ProviderMode::DirectLocal,
            latency_ms: 900,
            confidence: None,
            outcome: VisionProposalOutcome::Accepted,
        };
        metrics.record_vision_proposal(slow);
        metrics.record_vision_proposal(slow);
        assert_eq!(
            metrics.provider_health()[0].status,
            types::ProviderHealthStatus::Degraded
        );
        // No budget configured: slow calls stay healthy.
        let metrics = OperationalMetrics::default();
        metrics.record_vision_proposal(slow);
        assert_eq!(
            metrics.provider_health()[0].status,
            types::ProviderHealthStatus::Healthy
        );
    }
}
