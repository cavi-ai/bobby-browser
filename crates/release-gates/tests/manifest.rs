use release_gates::{ManifestError, ReleaseManifest, MANIFEST_SCHEMA_VERSION};

fn valid() -> Vec<u8> {
    br#"{
      "schemaVersion":1,
      "security":{"required":true,"timeoutSecs":900,"maxOutputBytes":1048576},
      "secretCanaries":["release-gate-secret-that-must-never-escape"]
    }"#
    .to_vec()
}

#[test]
fn parses_the_strict_versioned_manifest() {
    let manifest = ReleaseManifest::from_slice(&valid()).unwrap();
    assert_eq!(manifest.schema_version, MANIFEST_SCHEMA_VERSION);
    assert!(manifest.security.required);
}

#[test]
fn rejects_unknown_fields_versions_empty_canaries_and_zero_bounds() {
    for (needle, replacement, expected) in [
        (
            r#""schemaVersion":1"#,
            r#""schemaVersion":2"#,
            "unsupported schema version",
        ),
        (
            r#""timeoutSecs":900"#,
            r#""timeoutSecs":0"#,
            "timeoutSecs must be positive",
        ),
        (
            r#""maxOutputBytes":1048576"#,
            r#""maxOutputBytes":0"#,
            "maxOutputBytes must be positive",
        ),
        (
            r#""secretCanaries":["release-gate-secret-that-must-never-escape"]"#,
            r#""secretCanaries":[]"#,
            "secretCanaries must not be empty",
        ),
    ] {
        let input = String::from_utf8(valid())
            .unwrap()
            .replace(needle, replacement);
        assert!(ReleaseManifest::from_slice(input.as_bytes())
            .unwrap_err()
            .to_string()
            .contains(expected));
    }
    let unknown = String::from_utf8(valid())
        .unwrap()
        .replace("\n    }", ",\n      \"unknown\":true\n    }");
    assert!(matches!(
        ReleaseManifest::from_slice(unknown.as_bytes()),
        Err(ManifestError::Decode(_))
    ));
}
