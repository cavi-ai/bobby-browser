//! `behavioral-engine`, `fingerprinting`, and `task-scheduler` landed on main
//! without a capability reference or a `JsonSchema` derive between them, and
//! the node-substrate spec calls that out as an integration constraint to
//! settle before any of them reaches a surface.
//!
//! There are two ways to settle it. Either those crates get `JsonSchema`
//! derives so the schema parity guard covers their wire types, or they never
//! become wire types and the surface exposes only `types::` shapes the guard
//! already covers. This repo takes the second: a session opts in through two
//! booleans on `ExecutionPolicy`, and what the humanizer did comes back as
//! `Evidence::Humanization` — both in `types`, both already guarded.
//!
//! That choice is only safe while it stays true. This file is what keeps it
//! true: it fails the moment a schema starts naming a type from one of those
//! three crates, which is the point at which the derives stop being optional.

use serde_json::Value;

/// Type names from the three crates. A `$ref` or `title` naming any of these
/// means a schema is describing a shape whose definition lives outside
/// `types::`, and therefore outside the parity guard.
const OFF_WIRE_TYPE_NAMES: &[&str] = &[
    // behavioral-engine
    "BehavioralConfig",
    "MouseConfig",
    "MousePath",
    "MousePoint",
    "ScrollConfig",
    "ScrollAction",
    "TextConfig",
    "TypingAction",
    "SessionRandom",
    "BehavioralBenchmarkReport",
    "DimensionScore",
    // fingerprinting
    "FingerprintConfig",
    "FingerprintSession",
    "FingerprintApplyPlan",
    "CanvasConfig",
    "WebGlConfig",
    "WebGlProfile",
    "AudioConfig",
    "FontConfig",
    "ScreenConfig",
    "ScreenResolution",
    "ClientHintsProfile",
    "DeviceMetrics",
    // task-scheduler
    "Job",
    "JobConfig",
    "JobPriority",
    "JobStatus",
    "JobResult",
    "JobError",
    "SchedulerConfig",
    "SchedulerStats",
    "RetryConfig",
    "QueueStats",
];

fn walk(value: &Value, hit: &mut dyn FnMut(&str, &str)) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == "$ref" || key == "title" {
                    if let Some(text) = child.as_str() {
                        let name = text.rsplit('/').next().unwrap_or(text);
                        hit(key, name);
                    }
                }
                walk(child, hit);
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(item, hit);
            }
        }
        _ => {}
    }
}

#[test]
fn no_tool_schema_names_a_type_from_an_unguarded_crate() {
    let mut offences = Vec::new();
    // `command_execute` reaches the widest definition closure of any tool —
    // the full `PrimitiveCommand` and `IntentCommand` unions — so a leak from
    // any command shape surfaces here.
    for tool in ["command_execute", "session_create", "checkpoint_save"] {
        let schema = mcp_gateway::schema_for_test(tool);
        walk(&schema, &mut |key, name| {
            if OFF_WIRE_TYPE_NAMES.contains(&name) {
                offences.push(format!("{tool}: {key} -> {name}"));
            }
        });
    }
    // The unpatched definitions table, not just the emitted schemas: a type
    // can be defined here and reachable from a tool added later.
    let definitions = mcp_gateway::definitions_for_test();
    if let Some(map) = definitions.as_object() {
        for name in map.keys() {
            if OFF_WIRE_TYPE_NAMES.contains(&name.as_str()) {
                offences.push(format!("definitions(): {name}"));
            }
        }
    }
    assert!(
        offences.is_empty(),
        "a schema names a type from behavioral-engine, fingerprinting, or task-scheduler. \
         Those crates carry no JsonSchema derive, so the parity guard cannot see them: \
         either add the derives and extend tests/schema_parity.rs, or keep the wire shape \
         in types::. Offences: {offences:?}"
    );
}

/// The counterpart: the shapes that *do* carry the opt-in are `types::` ones,
/// and the surface advertises them. Without this, the test above could pass by
/// the feature not being exposed at all.
#[test]
fn the_session_opt_in_is_advertised_as_a_types_shape() {
    let schema = mcp_gateway::schema_for_test("session_create");
    let policy = &schema["properties"]["executionPolicy"]["properties"];
    for flag in [
        "javascriptEvaluation",
        "visionAssist",
        "fingerprint",
        "humanize",
    ] {
        assert_eq!(
            policy[flag]["type"], "boolean",
            "session_create does not advertise executionPolicy.{flag}: {schema}"
        );
    }
}
