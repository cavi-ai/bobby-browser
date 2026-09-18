//! Provider health tracking: consecutive-failure and latency-budget
//! enforcement for vision provider boundaries. Report-only — the tracker
//! classifies each provider mode `healthy` / `degraded` / `unhealthy` for
//! `/v1/runtime` and `bobby doctor`; it never changes escalation behavior.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub use types::{ProviderHealthSnapshot, ProviderHealthStatus};

use crate::ProviderMode;

const NO_LATENCY: u64 = u64::MAX;

/// Outcome class of one provider round-trip. A below-floor or unverified
/// proposal is still a `Success` here: the provider answered; the verdict is
/// tracked by the verification metrics, not provider health.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderCallOutcome {
    Success,
    Failure,
}

/// Process-local health for each vision provider mode. Clone-cheap; every
/// clone shares the same counters, mirroring [`crate::OperationalMetrics`].
#[derive(Clone)]
pub struct ProviderHealthTracker {
    inner: Arc<ProviderHealthInner>,
}

struct ProviderHealthInner {
    latency_budget_ms: Option<u64>,
    failure_threshold: u64,
    successes: [AtomicU64; 3],
    failures: [AtomicU64; 3],
    consecutive_failures: [AtomicU64; 3],
    budget_violations: [AtomicU64; 3],
    consecutive_budget_violations: [AtomicU64; 3],
    last_latency_ms: [AtomicU64; 3],
}

impl Default for ProviderHealthTracker {
    fn default() -> Self {
        Self::new(None, 3)
    }
}

impl ProviderHealthTracker {
    /// `failure_threshold` consecutive failures report `unhealthy`; the same
    /// count of consecutive latency-budget violations reports `degraded`.
    /// A threshold of 0 is clamped to 1 so health cannot trip on zero events.
    pub fn new(latency_budget_ms: Option<u64>, failure_threshold: u32) -> Self {
        Self {
            inner: Arc::new(ProviderHealthInner {
                latency_budget_ms,
                failure_threshold: u64::from(failure_threshold.max(1)),
                successes: atomic_array(),
                failures: atomic_array(),
                consecutive_failures: atomic_array(),
                budget_violations: atomic_array(),
                consecutive_budget_violations: atomic_array(),
                last_latency_ms: std::array::from_fn(|_| AtomicU64::new(NO_LATENCY)),
            }),
        }
    }

    pub fn record(&self, mode: ProviderMode, outcome: ProviderCallOutcome, latency_ms: u64) {
        let index = mode as usize;
        self.inner.last_latency_ms[index].store(latency_ms, Ordering::Release);
        match outcome {
            ProviderCallOutcome::Success => {
                increment(&self.inner.successes[index]);
                self.inner.consecutive_failures[index].store(0, Ordering::Release);
            }
            ProviderCallOutcome::Failure => {
                increment(&self.inner.failures[index]);
                increment(&self.inner.consecutive_failures[index]);
            }
        }
        let over_budget = self
            .inner
            .latency_budget_ms
            .is_some_and(|budget| latency_ms > budget);
        if over_budget {
            increment(&self.inner.budget_violations[index]);
            increment(&self.inner.consecutive_budget_violations[index]);
        } else {
            self.inner.consecutive_budget_violations[index].store(0, Ordering::Release);
        }
    }

    /// One snapshot per provider mode with recorded activity, ordered by
    /// mode. Modes that never served a call are omitted.
    pub fn snapshot(&self) -> Vec<ProviderHealthSnapshot> {
        [
            ProviderMode::Http,
            ProviderMode::Acp,
            ProviderMode::DirectLocal,
        ]
        .into_iter()
        .filter_map(|mode| self.snapshot_mode(mode))
        .collect()
    }

    fn snapshot_mode(&self, mode: ProviderMode) -> Option<ProviderHealthSnapshot> {
        let index = mode as usize;
        let successes = self.inner.successes[index].load(Ordering::Acquire);
        let failures = self.inner.failures[index].load(Ordering::Acquire);
        if successes.saturating_add(failures) == 0 {
            return None;
        }
        let consecutive_failures = self.inner.consecutive_failures[index].load(Ordering::Acquire);
        let budget_violations = self.inner.budget_violations[index].load(Ordering::Acquire);
        let consecutive_budget_violations =
            self.inner.consecutive_budget_violations[index].load(Ordering::Acquire);
        let last_latency_ms = match self.inner.last_latency_ms[index].load(Ordering::Acquire) {
            NO_LATENCY => None,
            value => Some(value),
        };
        let status = if consecutive_failures >= self.inner.failure_threshold {
            ProviderHealthStatus::Unhealthy
        } else if consecutive_budget_violations >= self.inner.failure_threshold {
            ProviderHealthStatus::Degraded
        } else {
            ProviderHealthStatus::Healthy
        };
        Some(ProviderHealthSnapshot {
            provider_mode: provider_mode_name(mode).to_string(),
            status,
            successes,
            failures,
            consecutive_failures,
            budget_violations,
            last_latency_ms,
            latency_budget_ms: self.inner.latency_budget_ms,
            failure_threshold: self.inner.failure_threshold,
        })
    }
}

pub fn provider_mode_name(mode: ProviderMode) -> &'static str {
    match mode {
        ProviderMode::Http => "http",
        ProviderMode::Acp => "acp",
        ProviderMode::DirectLocal => "directLocal",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inactive_modes_are_omitted_from_the_snapshot() {
        let tracker = ProviderHealthTracker::new(Some(1_000), 3);
        assert!(tracker.snapshot().is_empty());
        tracker.record(ProviderMode::Http, ProviderCallOutcome::Success, 100);
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].provider_mode, "http");
        assert_eq!(snapshot[0].status, ProviderHealthStatus::Healthy);
        assert_eq!(snapshot[0].successes, 1);
        assert_eq!(snapshot[0].last_latency_ms, Some(100));
        assert_eq!(snapshot[0].latency_budget_ms, Some(1_000));
        assert_eq!(snapshot[0].failure_threshold, 3);
    }

    #[test]
    fn consecutive_failures_trip_unhealthy_and_success_recovers() {
        let tracker = ProviderHealthTracker::new(None, 2);
        tracker.record(ProviderMode::Acp, ProviderCallOutcome::Failure, 10);
        assert_eq!(tracker.snapshot()[0].status, ProviderHealthStatus::Healthy);
        tracker.record(ProviderMode::Acp, ProviderCallOutcome::Failure, 10);
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot[0].status, ProviderHealthStatus::Unhealthy);
        assert_eq!(snapshot[0].consecutive_failures, 2);
        tracker.record(ProviderMode::Acp, ProviderCallOutcome::Success, 10);
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot[0].status, ProviderHealthStatus::Healthy);
        assert_eq!(snapshot[0].consecutive_failures, 0);
        assert_eq!(snapshot[0].failures, 2);
        assert_eq!(snapshot[0].successes, 1);
    }

    #[test]
    fn consecutive_budget_violations_trip_degraded() {
        let tracker = ProviderHealthTracker::new(Some(500), 2);
        tracker.record(ProviderMode::DirectLocal, ProviderCallOutcome::Success, 600);
        assert_eq!(tracker.snapshot()[0].status, ProviderHealthStatus::Healthy);
        tracker.record(ProviderMode::DirectLocal, ProviderCallOutcome::Success, 700);
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot[0].status, ProviderHealthStatus::Degraded);
        assert_eq!(snapshot[0].budget_violations, 2);
        tracker.record(ProviderMode::DirectLocal, ProviderCallOutcome::Success, 100);
        assert_eq!(tracker.snapshot()[0].status, ProviderHealthStatus::Healthy);
    }

    #[test]
    fn failures_outrank_budget_violations() {
        let tracker = ProviderHealthTracker::new(Some(500), 2);
        tracker.record(ProviderMode::Http, ProviderCallOutcome::Success, 900);
        tracker.record(ProviderMode::Http, ProviderCallOutcome::Failure, 900);
        tracker.record(ProviderMode::Http, ProviderCallOutcome::Failure, 900);
        assert_eq!(
            tracker.snapshot()[0].status,
            ProviderHealthStatus::Unhealthy
        );
    }

    #[test]
    fn zero_threshold_is_clamped() {
        let tracker = ProviderHealthTracker::new(None, 0);
        tracker.record(ProviderMode::Http, ProviderCallOutcome::Failure, 1);
        assert_eq!(
            tracker.snapshot()[0].status,
            ProviderHealthStatus::Unhealthy
        );
    }

    #[test]
    fn provider_mode_wire_names_are_stable() {
        assert_eq!(provider_mode_name(ProviderMode::Http), "http");
        assert_eq!(provider_mode_name(ProviderMode::Acp), "acp");
        assert_eq!(provider_mode_name(ProviderMode::DirectLocal), "directLocal");
    }
}
