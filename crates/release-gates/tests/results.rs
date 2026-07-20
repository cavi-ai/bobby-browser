use release_gates::{GateObservation, GateResult, GateStatus, ResultError};

#[test]
fn result_digest_is_stable_and_redaction_covers_every_text_surface() {
    let mut result = GateResult::new(
        "security",
        "secret-check",
        true,
        GateStatus::Blocked,
        17,
        vec![
            GateObservation::new("stderr", "token=alpha-secret"),
            GateObservation::new("alpha-secret observation", "plain text"),
        ],
    );
    result.diagnostics = "alpha-secret in diagnostic".into();
    let evidence_before_redaction = result.evidence_sha256.clone();

    result.redact(&["alpha-secret".into()]);

    let json = serde_json::to_string(&result).unwrap();
    assert!(!json.contains("alpha-secret"));
    assert!(json.contains("[REDACTED]"));
    assert_ne!(result.evidence_sha256, evidence_before_redaction);
    assert_eq!(result.digest_hex(), result.digest_hex());
    assert_eq!(result.digest_hex().len(), 64);
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
