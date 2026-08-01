//! Drift guard: the hand-bounded MCP tool schemas must advertise exactly the
//! same command and evidence variants the Rust wire types serialize.

use std::collections::BTreeSet;

use serde_json::Value;

fn schemars_kinds(schema: &Value) -> BTreeSet<String> {
    // Top-level tagged variants only: schemars emits internally tagged enums as
    // `oneOf` (or nested `anyOf`/`allOf` wrappers) whose items pin the tag
    // property as a const. Nested enums inside variants have their own oneOf,
    // so walking only the first oneOf layer keeps the comparison at the same
    // granularity as the hand-written schema.
    let Some(variants) = schema["oneOf"].as_array() else {
        panic!("expected a oneOf variant list: {schema}");
    };
    variants
        .iter()
        .map(|variant| {
            variant["properties"]["kind"]["const"]
                .as_str()
                .unwrap_or_else(|| panic!("variant must pin a kind const: {variant}"))
                .to_owned()
        })
        .collect()
}

/// `tool` names the tool whose schema reaches `def`. Each tool now carries only
/// the definitions it can actually reach, so command variants come from
/// `command_execute` and evidence variants from `checkpoint_save`.
fn hand_kinds(tool: &str, def: &str) -> BTreeSet<String> {
    let schema = mcp_gateway::schema_for_test(tool);
    schema["$defs"][def]["oneOf"]
        .as_array()
        .unwrap_or_else(|| panic!("{def} oneOf must be an array"))
        .iter()
        .map(|variant| {
            variant["properties"]["kind"]["const"]
                .as_str()
                .unwrap_or_else(|| panic!("{def} variant must pin a kind const"))
                .to_owned()
        })
        .collect()
}

#[test]
fn primitive_command_variants_match_the_wire_type() {
    let generated = schemars_kinds(
        &serde_json::to_value(schemars::schema_for!(types::PrimitiveCommand)).unwrap(),
    );
    assert_eq!(generated, hand_kinds("command_execute", "PrimitiveCommand"));
}

#[test]
fn intent_command_variants_match_the_wire_type() {
    let generated =
        schemars_kinds(&serde_json::to_value(schemars::schema_for!(types::IntentCommand)).unwrap());
    assert_eq!(generated, hand_kinds("command_execute", "IntentCommand"));
}

#[test]
fn evidence_variants_match_the_wire_type() {
    let generated =
        schemars_kinds(&serde_json::to_value(schemars::schema_for!(types::Evidence)).unwrap());
    assert_eq!(generated, hand_kinds("checkpoint_save", "Evidence"));
}
