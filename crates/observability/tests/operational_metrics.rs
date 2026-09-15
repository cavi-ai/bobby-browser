use observability::{
    ContextCandidateRankingMetric, ContextLookupOutcome, ContextRankedVisionMetric,
    IntentMetricKind, OperationalMetrics, PrefillOutcome, ProviderMode,
    ReconciliationMetricOutcome, ResolutionSource, RetryClass, StructuralContextSource,
    VerificationMetricResult, VisionProposalMetric, VisionProposalOutcome, WorkflowCallClass,
};

#[test]
fn snapshot_records_bounded_operational_outcomes() {
    let metrics = OperationalMetrics::default();

    metrics.record_intent_resolution(IntentMetricKind::Fill, ResolutionSource::VisionFallback);
    metrics.record_context_lookup(ContextLookupOutcome::Miss);
    metrics.record_prefill(PrefillOutcome::DroppedEntry);
    metrics.record_vision_proposal(VisionProposalMetric {
        provider_mode: ProviderMode::Http,
        latency_ms: 250,
        confidence: Some(0.90),
        outcome: VisionProposalOutcome::Accepted,
    });
    metrics.record_verification(VerificationMetricResult::Accepted);
    metrics.record_retry(RetryClass::Transport);
    metrics.record_reconciliation(ReconciliationMetricOutcome::NeedsReconciliation);
    metrics.record_workflow_call(WorkflowCallClass::CompositeWorkflow);

    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.intent.total, 1);
    assert_eq!(snapshot.intent.fill, 1);
    assert_eq!(snapshot.intent.vision_fallback, 1);
    assert_eq!(snapshot.context.miss, 1);
    assert_eq!(snapshot.prefill.dropped_entry, 1);
    assert_eq!(snapshot.vision.attempted, 1);
    assert_eq!(snapshot.vision.accepted, 1);
    assert_eq!(snapshot.vision.provider_http, 1);
    assert_eq!(snapshot.vision.latency_ms.buckets[3].upper_bound_ms, 250);
    assert_eq!(snapshot.vision.latency_ms.buckets[3].count, 1);
    assert_eq!(snapshot.vision.confidence.high, 1);
    assert_eq!(snapshot.verification.accepted, 1);
    assert_eq!(snapshot.retries.transport, 1);
    assert_eq!(snapshot.reconciliation.needs_reconciliation, 1);
    assert_eq!(snapshot.workflow_calls.composite_workflow, 1);
}

#[test]
fn latency_histogram_has_exact_inclusive_boundaries_and_overflow() {
    let metrics = OperationalMetrics::default();
    let samples = [
        0, 25, 26, 50, 51, 100, 101, 250, 251, 500, 501, 1_000, 1_001, 2_500, 2_501, 5_000, 5_001,
        10_000, 10_001, 15_000, 15_001,
    ];
    for latency_ms in samples {
        metrics.record_vision_proposal(VisionProposalMetric {
            provider_mode: ProviderMode::DirectLocal,
            latency_ms,
            confidence: None,
            outcome: VisionProposalOutcome::Failed,
        });
    }

    let histogram = metrics.snapshot().vision.latency_ms;
    assert_eq!(
        histogram
            .buckets
            .iter()
            .map(|bucket| (bucket.upper_bound_ms, bucket.count))
            .collect::<Vec<_>>(),
        vec![
            (25, 2),
            (50, 2),
            (100, 2),
            (250, 2),
            (500, 2),
            (1_000, 2),
            (2_500, 2),
            (5_000, 2),
            (10_000, 2),
            (15_000, 2),
        ]
    );
    assert_eq!(histogram.overflow, 1);
}

#[test]
fn confidence_bands_use_acceptance_and_high_boundaries() {
    let metrics = OperationalMetrics::default();
    for confidence in [0.0, 0.74, 0.75, 0.89, 0.90, 1.0] {
        metrics.record_vision_proposal(VisionProposalMetric {
            provider_mode: ProviderMode::Acp,
            latency_ms: 1,
            confidence: Some(confidence),
            outcome: VisionProposalOutcome::Rejected,
        });
    }

    let bands = metrics.snapshot().vision.confidence;
    assert_eq!(bands.below_acceptance, 2);
    assert_eq!(bands.accepted, 2);
    assert_eq!(bands.high, 2);
    assert_eq!(bands.unreported, 0);
}

#[test]
fn concurrent_updates_are_not_lost() {
    let metrics = OperationalMetrics::default();
    let threads = (0..8)
        .map(|_| {
            let metrics = metrics.clone();
            std::thread::spawn(move || {
                for _ in 0..1_000 {
                    metrics.record_workflow_call(WorkflowCallClass::Read);
                }
            })
        })
        .collect::<Vec<_>>();

    for thread in threads {
        thread.join().unwrap();
    }

    assert_eq!(metrics.snapshot().workflow_calls.read, 8_000);
}

#[test]
fn serialized_snapshot_cannot_retain_sensitive_canaries() {
    let metrics = OperationalMetrics::default();
    let canaries = [
        "prompt-canary-4b69",
        "typed-canary-8f03",
        "https://secret.invalid/canary",
        "candidate-name-canary",
        "bearer-canary-31c7",
        "raw-provider-error-canary",
        "workflow-id-canary",
    ];

    metrics.record_vision_proposal(VisionProposalMetric {
        provider_mode: ProviderMode::Http,
        latency_ms: 15_001,
        confidence: None,
        outcome: VisionProposalOutcome::TimedOut,
    });
    metrics.record_context_candidate_ranking(ContextCandidateRankingMetric {
        source: Some(StructuralContextSource::VisionPromoted),
        outcome: ContextLookupOutcome::StaleRejection,
        latency_ms: 26,
    });
    metrics.record_context_ranked_vision(ContextRankedVisionMetric {
        provider_mode: ProviderMode::Http,
        confidence: Some(0.74),
        verification: Some(VerificationMetricResult::OtherRejected),
    });
    let serialized = serde_json::to_string(&metrics.snapshot()).unwrap();

    for canary in canaries {
        assert!(
            !serialized.contains(canary),
            "metric snapshot leaked {canary}"
        );
    }
    assert!(serialized.contains("timedOut"));
    assert!(serialized.contains("observationWindowMs"));
    assert!(serialized.contains("contextRankedVision"));
    for forbidden_key in [
        "accessibleName",
        "candidateName",
        "domValue",
        "payload",
        "purpose",
        "selector",
        "url",
    ] {
        assert!(
            !serialized.contains(forbidden_key),
            "metric snapshot exposed forbidden field {forbidden_key}"
        );
    }
}

#[test]
fn context_ranked_vision_snapshot_has_a_bounded_contract() {
    let metrics = OperationalMetrics::default();
    metrics.record_context_candidate_ranking(ContextCandidateRankingMetric {
        source: Some(StructuralContextSource::Observed),
        outcome: ContextLookupOutcome::Hit,
        latency_ms: 25,
    });
    metrics.record_context_ranked_vision(ContextRankedVisionMetric {
        provider_mode: ProviderMode::Acp,
        confidence: Some(0.90),
        verification: Some(VerificationMetricResult::Accepted),
    });

    let snapshot = metrics.snapshot().context_ranked_vision;
    assert_eq!(snapshot.attempted, 1);
    assert_eq!(snapshot.source_observed, 1);
    assert_eq!(snapshot.source_vision_promoted, 0);
    assert_eq!(snapshot.source_unreported, 0);
    assert_eq!(snapshot.hit, 1);
    assert_eq!(snapshot.miss, 0);
    assert_eq!(snapshot.stale_rejection, 0);
    assert_eq!(snapshot.provider_escalations, 1);
    assert_eq!(snapshot.provider_acp, 1);
    assert_eq!(snapshot.confidence.high, 1);
    assert_eq!(snapshot.verification_accepted, 1);
    assert_eq!(snapshot.verification_rejected, 0);
    assert_eq!(snapshot.candidate_ranking_latency_ms.buckets[0].count, 1);

    let serialized = serde_json::to_value(snapshot).unwrap();
    let mut keys = serialized
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "ambiguousRefusal",
            "attempted",
            "candidateRankingLatencyMs",
            "confidence",
            "error",
            "hit",
            "miss",
            "providerAcp",
            "providerDirectLocal",
            "providerEscalations",
            "providerHttp",
            "sourceObserved",
            "sourceUnreported",
            "sourceVisionPromoted",
            "staleRejection",
            "verificationAccepted",
            "verificationRejected",
        ]
    );
}
