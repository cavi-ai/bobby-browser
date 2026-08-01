#![cfg(feature = "schema")]

use types::FormSnapshot;

#[test]
fn form_snapshot_schema_is_closed_versioned_and_bounded() {
    let schema = serde_json::to_value(schemars::schema_for!(FormSnapshot)).unwrap();
    let text = serde_json::to_string(&schema).unwrap();

    assert!(text.contains("schemaVersion"));
    assert!(text.contains("additionalProperties"));
    assert!(text.contains("maxItems"));
    assert!(text.contains("unownedControls"));
}
