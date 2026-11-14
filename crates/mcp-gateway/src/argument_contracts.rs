//! Arguments for `intent_follow` and credential fill.
//!
//! Advertised schema, dispatch, the wire command, and the argument struct
//! read this module. Checkers compare the TypeScript contract and
//! `skill/SKILL.md` against the same statement.

use serde_json::{json, Map, Value};

/// MCP spelling that the wire command stores.
pub const EXPECTED_DESTINATION: &str = "expectedDestination";
/// MCP spelling accepted in place of [`EXPECTED_DESTINATION`].
pub const EXPECTED_STATE: &str = "expectedState";

/// Both spellings. Exactly one is required.
pub const FOLLOW_WAIT_FIELDS: [&str; 2] = [EXPECTED_DESTINATION, EXPECTED_STATE];

/// `not` rejects this pair first (both present), then both absent.
const FOLLOW_EXCLUSION_ORDER: [&str; 2] = [EXPECTED_STATE, EXPECTED_DESTINATION];

/// Sentence the skill and the TypeScript contract must carry unchanged.
/// Checkers compare those files against it; production code does not call it.
#[cfg_attr(not(test), allow(dead_code))]
pub const FOLLOW_EXACTLY_ONE_RULE: &str = "exactly one of `expectedState` or `expectedDestination` (both spellings are accepted; both or neither is rejected)";

pub const FOLLOW_WAIT_DESCRIPTION: &str = "One wait. Example: {\"condition\":{\"kind\":\"networkQuiet\",\"idleMs\":200,\"maxInFlight\":0},\"timeoutMs\":10000}";

/// Wire `follow` input required fields. Matches `FollowIntent` after one
/// spelling has been chosen.
pub const FOLLOW_WIRE_REQUIRED: &[&str] = &["purpose", EXPECTED_DESTINATION];

pub const INTENT_FOLLOW_MISSING_EXPECTED_STATE_MESSAGE: &str = "intent_follow needs exactly one of \
    expectedState or expectedDestination (a WaitForCommand: {condition, timeoutMs}); e.g. \
    expectedState: {\"condition\":{\"kind\":\"url\",\"matcher\":{\"kind\":\"contains\",\"value\":\"/next\"}},\"timeoutMs\":10000}";

pub const CREDENTIAL_KIND: &str = "credential";
pub const CREDENTIAL_NAME_MIN: usize = 1;
pub const CREDENTIAL_NAME_MAX: usize = types::MAX_CREDENTIAL_NAME_BYTES;
pub const CREDENTIAL_FILL_REF: &str = "#/$defs/CredentialFill";

pub fn follow_wait_property() -> Value {
    json!({"$ref":"#/$defs/WaitForCommand"})
}

/// Both MCP spellings of the follow wait. Same schema object for each.
pub fn follow_wait_properties() -> Value {
    let wait = follow_wait_property();
    let mut properties = Map::new();
    for field in FOLLOW_WAIT_FIELDS {
        properties.insert((*field).to_owned(), wait.clone());
    }
    Value::Object(properties)
}

/// Advertised `not`: both spellings set, or neither set.
pub fn follow_exactly_one_not() -> Value {
    let mut absent = Map::new();
    for field in FOLLOW_WAIT_FIELDS {
        absent.insert((*field).to_owned(), Value::Bool(false));
    }
    json!({
        "oneOf": [
            {"required": FOLLOW_EXCLUSION_ORDER.to_vec()},
            {"properties": Value::Object(absent)}
        ]
    })
}

/// Wire `follow` input properties. The stored field is [`EXPECTED_DESTINATION`].
pub fn follow_wire_properties(purpose: Value, hints: Value, boundary: Value) -> Value {
    let mut properties = Map::new();
    properties.insert("purpose".to_owned(), purpose);
    properties.insert("hints".to_owned(), hints);
    properties.insert(EXPECTED_DESTINATION.to_owned(), follow_wait_property());
    properties.insert("boundary".to_owned(), boundary);
    Value::Object(properties)
}

/// Exactly one spelling. `Err` is [`INTENT_FOLLOW_MISSING_EXPECTED_STATE_MESSAGE`].
pub fn exactly_one_follow_wait<T>(
    expected_destination: Option<T>,
    expected_state: Option<T>,
) -> Result<T, &'static str> {
    match (expected_destination, expected_state) {
        (Some(wait), None) | (None, Some(wait)) => Ok(wait),
        _ => Err(INTENT_FOLLOW_MISSING_EXPECTED_STATE_MESSAGE),
    }
}

/// `{kind:"credential", name}` with the shared name bound.
pub fn credential_fill_schema(name: Value) -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "kind": {"const": CREDENTIAL_KIND},
            "name": name
        },
        "required": ["kind", "name"]
    })
}

pub fn credential_fill_ref() -> Value {
    json!({"$ref": CREDENTIAL_FILL_REF})
}

/// TypeScript `ControlAction` variant this schema describes.
/// Checkers compare `contracts.ts` against it; production code does not call it.
#[cfg_attr(not(test), allow(dead_code))]
pub fn credential_fill_typescript() -> String {
    format!("{{ kind: \"{CREDENTIAL_KIND}\"; name: string }}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    #[test]
    fn skill_and_typescript_state_the_follow_and_credential_contracts() {
        let root = repo_root();
        let skill = std::fs::read_to_string(root.join("skill/SKILL.md")).unwrap();
        let contracts =
            std::fs::read_to_string(root.join("packages/typescript-sdk/src/contracts.ts")).unwrap();
        assert!(
            skill.contains(FOLLOW_EXACTLY_ONE_RULE),
            "skill/SKILL.md must state the follow rule from this module"
        );
        assert!(
            contracts.contains(FOLLOW_EXACTLY_ONE_RULE),
            "FollowIntent must state the follow rule from this module"
        );
        assert!(
            contracts.contains(&credential_fill_typescript()),
            "ControlAction credential variant drifted from {}",
            credential_fill_typescript()
        );
    }

    #[test]
    fn exactly_one_follow_wait_accepts_either_spelling_only() {
        assert_eq!(exactly_one_follow_wait(Some(1), None).unwrap(), 1);
        assert_eq!(exactly_one_follow_wait(None, Some(2)).unwrap(), 2);
        assert_eq!(
            exactly_one_follow_wait::<u8>(None, None).unwrap_err(),
            INTENT_FOLLOW_MISSING_EXPECTED_STATE_MESSAGE
        );
        assert_eq!(
            exactly_one_follow_wait(Some(1), Some(2)).unwrap_err(),
            INTENT_FOLLOW_MISSING_EXPECTED_STATE_MESSAGE
        );
    }
}
