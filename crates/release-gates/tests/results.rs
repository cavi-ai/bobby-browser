use release_gates::{GateObservation, GateResult, GateStatus, ResultError, RESULT_SCHEMA_VERSION};
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EvidencePreimage<'a> {
    schema_version: u32,
    suite: &'a str,
    check: &'a str,
    required: bool,
    status: &'a GateStatus,
    duration_ms: u64,
    observations: &'a [GateObservation],
    diagnostics: &'a str,
}

fn expected_evidence_sha256(result: &GateResult) -> String {
    let preimage = serde_json::to_vec(&EvidencePreimage {
        schema_version: RESULT_SCHEMA_VERSION,
        suite: &result.suite,
        check: &result.check,
        required: result.required,
        status: &result.status,
        duration_ms: result.duration_ms,
        observations: &result.observations,
        diagnostics: &result.diagnostics,
    })
    .unwrap();
    format!("{:x}", Sha256::digest(preimage))
}

#[test]
fn result_digest_is_stable_and_redaction_covers_every_text_surface() {
    let mut result = GateResult::new(
        "alpha-secret suite",
        "alpha-secret check",
        true,
        GateStatus::Blocked,
        17,
        vec![
            GateObservation::new("stderr", "token=alpha-secret"),
            GateObservation::new("alpha-secret observation", "plain text"),
        ],
    );
    result.diagnostics = "alpha-secret in diagnostic".into();
    let evidence_before_redaction = result.evidence_sha256().unwrap();

    result.redact(&["alpha-secret".into()]);

    let json = serde_json::to_string(&result).unwrap();
    assert!(!json.contains("alpha-secret"));
    assert!(json.contains("[REDACTED]"));
    assert_ne!(result.evidence_sha256().unwrap(), evidence_before_redaction);
    assert_eq!(
        result.evidence_sha256().unwrap(),
        expected_evidence_sha256(&result)
    );
    let serialized = serde_json::to_vec(&result).unwrap();
    assert_eq!(
        result.digest_hex().unwrap(),
        format!("{:x}", Sha256::digest(serialized))
    );
    assert_eq!(result.digest_hex().unwrap().len(), 64);
}

#[test]
fn serialization_recomputes_evidence_after_public_field_mutation() {
    let mut result = GateResult::new("security", "check", true, GateStatus::Passed, 1, vec![]);
    let evidence_before_mutation = result.evidence_sha256().unwrap();
    result.suite = "changed suite".into();
    result.check = "changed check".into();
    result.diagnostics = "changed diagnostics".into();

    let serialized = serde_json::to_vec(&result).unwrap();
    let parsed: GateResult = serde_json::from_slice(&serialized).unwrap();

    assert_ne!(parsed.evidence_sha256().unwrap(), evidence_before_mutation);
    assert_eq!(
        parsed.evidence_sha256().unwrap(),
        expected_evidence_sha256(&parsed)
    );
}

#[test]
fn deserialization_rejects_forged_evidence_and_unsupported_schema_versions() {
    let result = GateResult::new("security", "check", true, GateStatus::Passed, 1, vec![]);
    let mut forged: serde_json::Value = serde_json::to_value(&result).unwrap();
    forged["evidenceSha256"] = serde_json::Value::String("0".repeat(64));
    assert!(serde_json::from_value::<GateResult>(forged)
        .unwrap_err()
        .to_string()
        .contains("evidenceSha256 does not match canonical evidence"));

    let mut unsupported: serde_json::Value = serde_json::to_value(&result).unwrap();
    unsupported["schemaVersion"] = serde_json::Value::from(2);
    assert!(serde_json::from_value::<GateResult>(unsupported)
        .unwrap_err()
        .to_string()
        .contains("unsupported gate result schema version 2; expected 1"));
}

#[test]
fn persisted_json_is_bounded_and_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("result.json");
    GateResult::new("security", "ok", true, GateStatus::Passed, 1, vec![])
        .write_json(&path, 4096)
        .unwrap();
    let parsed: GateResult = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(parsed.status, GateStatus::Passed);
}

#[test]
fn write_json_rejects_oversized_results_without_creating_a_destination() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("result.json");
    let result = GateResult::new("security", "ok", true, GateStatus::Passed, 1, vec![]);

    assert!(matches!(
        result.write_json(&path, 1),
        Err(ResultError::TooLarge { .. })
    ));
    assert!(!path.exists());
}

#[test]
fn write_json_does_not_replace_an_existing_temporary_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("result.json");
    let temporary_path = path.with_extension("json.tmp");
    std::fs::write(&temporary_path, b"existing temporary result").unwrap();
    let result = GateResult::new("security", "ok", true, GateStatus::Passed, 1, vec![]);

    assert!(matches!(
        result.write_json(&path, 4096),
        Err(ResultError::Io(_))
    ));
    assert_eq!(
        std::fs::read(&temporary_path).unwrap(),
        b"existing temporary result"
    );
    assert!(!path.exists());
}
