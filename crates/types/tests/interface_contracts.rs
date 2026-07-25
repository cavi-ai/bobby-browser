use chrono::{Duration, Utc};
use serde_json::json;
use types::{
    Capability, CapabilitySet, ErrorLayer, IdempotencyKey, InterfaceError, InterfaceErrorCode,
    InterfaceOperation, InterfaceVersion, PrincipalId, RequestContext,
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
    assert_eq!(json["interfaceVersion"], "2026-07-23");
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
