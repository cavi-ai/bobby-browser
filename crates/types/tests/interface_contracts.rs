use chrono::{Duration, Utc};
use serde_json::json;
use types::{
    Capability, CapabilitySet, ErrorLayer, IdempotencyKey, InterfaceError, InterfaceErrorCode,
    InterfaceOperation, InterfaceVersion, PrincipalId, RequestContext, RuntimeInfo,
    CURRENT_INTERFACE_VERSION,
};
use uuid::Uuid;

#[test]
fn request_context_and_errors_have_stable_wire_contracts() {
    let context = RequestContext::new_for_test(
        PrincipalId::from_uuid(Uuid::from_u128(0x10000000000000000000000000000001)),
        [Capability::SessionRead, Capability::PageWrite],
        Utc::now() + Duration::seconds(30),
    );

    let json = serde_json::to_value(&context).unwrap();
    assert_eq!(json["interfaceVersion"], CURRENT_INTERFACE_VERSION);
    assert_eq!(json["capabilities"], json!(["page:write", "session:read"]));
    assert!(json.get("bearerToken").is_none());

    let error = InterfaceError {
        code: InterfaceErrorCode::MissingCapability,
        layer: ErrorLayer::Interface,
        message: "capability denied".into(),
        correlation_id: context.correlation_id.clone(),
        command_id: None,
        retryable: false,
        retry_after_ms: None,
        reconciliation_required: false,
        required_capability: Some(Capability::BrowserMutate),
    };
    assert_eq!(
        serde_json::to_value(error).unwrap(),
        json!({
            "code": "missingCapability",
            "layer": "interface",
            "message": "capability denied",
            "correlationId": context.correlation_id,
            "commandId": null,
            "retryable": false,
            "retryAfterMs": null,
            "reconciliationRequired": false,
            "requiredCapability": "browser:mutate",
        })
    );
}

#[test]
fn every_operation_capability_is_stable_and_fail_closed() {
    assert_eq!(
        InterfaceOperation::SubmitCommand.required(),
        &[Capability::BrowserMutate]
    );
    assert_eq!(
        InterfaceOperation::ReadArtifact.required(),
        &[Capability::ArtifactRead]
    );
    assert_eq!(
        InterfaceOperation::SubmitJob.required(),
        &[Capability::JobSubmit]
    );
    assert_eq!(
        InterfaceOperation::ReadJob.required(),
        &[Capability::JobRead]
    );
    assert_eq!(
        InterfaceOperation::CancelJob.required(),
        &[Capability::JobCancel]
    );
    assert_eq!(
        InterfaceOperation::IssuePrincipal.required(),
        &[Capability::AuthorityAdmin]
    );
    assert_eq!(
        InterfaceOperation::RevokePrincipal.required(),
        &[Capability::AuthorityAdmin]
    );
    assert!(!CapabilitySet::default().allows(InterfaceOperation::RuntimeInfo));
}

#[test]
fn interface_inputs_reject_unsupported_values_before_dispatch() {
    assert!(InterfaceVersion::try_from("2026-07-16").is_err());
    assert!(IdempotencyKey::try_from("").is_err());
    assert!(IdempotencyKey::try_from("line\nbreak").is_err());
    assert!(IdempotencyKey::try_from("x".repeat(129)).is_err());

    let context = RequestContext::new_for_test(
        PrincipalId::from_uuid(Uuid::from_u128(0x10000000000000000000000000000001)),
        [Capability::SessionRead],
        Utc::now(),
    );
    assert!(context.validate_at(Utc::now()).is_err());
}

#[test]
fn idempotency_conflict_has_a_stable_wire_code() {
    assert_eq!(
        serde_json::to_value(InterfaceErrorCode::IdempotencyConflict).unwrap(),
        json!("idempotencyConflict")
    );
}

#[test]
fn runtime_info_accepts_older_payloads_without_operational_metrics() {
    let info: RuntimeInfo = serde_json::from_value(json!({
        "version": "0.8.0",
        "capabilities": ["sdk"],
        "active_sessions": 0,
        "queued_jobs": 0,
        "uptime_ms": 42
    }))
    .unwrap();

    assert!(info.operational_metrics.is_none());
    assert!(serde_json::to_value(info)
        .unwrap()
        .get("operationalMetrics")
        .is_none());
}

#[test]
fn runtime_info_accepts_older_payloads_without_provider_health() {
    let info: RuntimeInfo = serde_json::from_value(json!({
        "version": "0.14.0",
        "capabilities": ["sdk"],
        "active_sessions": 0,
        "queued_jobs": 0,
        "uptime_ms": 42
    }))
    .unwrap();

    assert!(info.provider_health.is_none());
    assert!(serde_json::to_value(info)
        .unwrap()
        .get("providerHealth")
        .is_none());
}

#[test]
fn provider_health_snapshot_uses_camel_case_wire_names() {
    let snapshot: types::ProviderHealthSnapshot = serde_json::from_value(json!({
        "providerMode": "http",
        "status": "degraded",
        "successes": 10,
        "failures": 2,
        "consecutiveFailures": 0,
        "budgetViolations": 4,
        "lastLatencyMs": 1900,
        "latencyBudgetMs": 1500,
        "failureThreshold": 3
    }))
    .unwrap();

    assert_eq!(snapshot.status, types::ProviderHealthStatus::Degraded);
    assert_eq!(snapshot.last_latency_ms, Some(1900));
    let value = serde_json::to_value(snapshot).unwrap();
    assert_eq!(value.get("providerMode").unwrap(), &json!("http"));
    assert_eq!(value.get("budgetViolations").unwrap(), &json!(4));
}

#[test]
fn operational_metrics_accept_an_older_snapshot_without_context_ranked_vision() {
    let snapshot: types::OperationalMetricsSnapshot = serde_json::from_value(json!({
        "observationWindowMs": 1,
        "intent": {
            "total": 0, "locate": 0, "fill": 0, "completeForm": 0, "extract": 0,
            "submit": 0, "waitForState": 0, "follow": 0, "dismiss": 0,
            "solveChallenge": 0, "detectChallenge": 0, "deterministic": 0,
            "context": 0, "visionPrefill": 0, "visionFallback": 0
        },
        "context": {
            "hit": 0, "miss": 0, "ambiguousRefusal": 0, "staleRejection": 0,
            "error": 0
        },
        "prefill": {
            "hit": 0, "miss": 0, "droppedEntry": 0, "policyDenied": 0,
            "providerFailure": 0
        },
        "vision": {
            "attempted": 0, "accepted": 0, "rejected": 0, "abstained": 0,
            "timedOut": 0, "failed": 0, "providerHttp": 0, "providerAcp": 0,
            "providerDirectLocal": 0, "latencyMs": {"buckets": [], "overflow": 0},
            "confidence": {"belowAcceptance": 0, "accepted": 0, "high": 0, "unreported": 0}
        },
        "verification": {
            "accepted": 0, "targetNotFound": 0, "targetAmbiguous": 0,
            "obstructionPersisted": 0, "valueMismatch": 0, "otherRejected": 0
        },
        "retries": {
            "transport": 0, "timeout": 0, "targetDetached": 0,
            "stateConflict": 0, "other": 0
        },
        "reconciliation": {
            "resumed": 0, "restarted": 0, "needsReconciliation": 0, "failed": 0
        },
        "workflowCalls": {
            "lifecycle": 0, "discovery": 0, "read": 0, "mutation": 0,
            "compositeWorkflow": 0, "recovery": 0, "artifact": 0, "job": 0
        }
    }))
    .unwrap();
    assert_eq!(snapshot.context_ranked_vision.attempted, 0);
}
