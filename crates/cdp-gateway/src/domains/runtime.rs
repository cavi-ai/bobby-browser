use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{CdpError, CdpErrorCode};

const PLAYWRIGHT_1_61_INJECTED: (usize, &str) = (
    311_362,
    "219b161932480469c7ebe3baf2d66a8101276625916b6a60ad4e57f4858eb6cc",
);
const PLAYWRIGHT_1_62_INJECTED: (usize, &str) = (
    316_703,
    "60769c02b1d2f781a31f54e504ad2ee5fdf397b4fe47b43e7284f6d4e231a771",
);
const PLAYWRIGHT_1_61_UTILITY: (usize, &str) = (
    10_652,
    "3fc2ec24a4359c88a30f650d4daf23c0554eda935fb19bf1fe81f687f65d8dcd",
);
const PLAYWRIGHT_1_62_UTILITY: (usize, &str) = (
    11_035,
    "fd3b882cab3a898b34e827ab330d9fb152340d61f7d0c3cec30247018af131e4",
);

fn is_pinned_bootstrap_identity(len: usize, digest: &str) -> bool {
    [
        PLAYWRIGHT_1_61_INJECTED,
        PLAYWRIGHT_1_62_INJECTED,
        PLAYWRIGHT_1_61_UTILITY,
        PLAYWRIGHT_1_62_UTILITY,
    ]
    .contains(&(len, digest))
}

fn normalize_playwright_frame_seq(expression: &str) -> Option<String> {
    const MARKER: &str = r#""frameSeq":"#;
    let marker_start = expression.find(MARKER)?;
    if expression[marker_start + MARKER.len()..].contains(MARKER) {
        return None;
    }
    let value_start = marker_start + MARKER.len();
    let value_len = expression[value_start..]
        .bytes()
        .take_while(u8::is_ascii_digit)
        .count();
    if value_len == 0 {
        return None;
    }
    let value_end = value_start + value_len;
    let _: u32 = expression[value_start..value_end].parse().ok()?;
    let mut normalized = String::with_capacity(expression.len() - value_len + 1);
    normalized.push_str(&expression[..value_start]);
    normalized.push('0');
    normalized.push_str(&expression[value_end..]);
    Some(normalized)
}

/// Recognizes Playwright's pinned injected-script bootstrap without executing caller JavaScript.
pub(crate) fn bootstrap_injected_script(params: &Value) -> Result<Value, CdpError> {
    let expression = params
        .get("expression")
        .and_then(Value::as_str)
        .ok_or_else(|| CdpError::new(CdpErrorCode::InvalidParams, "missing runtime expression"))?;
    let digest = format!("{:x}", Sha256::digest(expression.as_bytes()));
    let identity = (expression.len(), digest.as_str());
    let normalized = normalize_playwright_frame_seq(expression);
    let normalized_digest = normalized
        .as_ref()
        .map(|value| format!("{:x}", Sha256::digest(value.as_bytes())));
    let normalized_identity = normalized
        .as_ref()
        .zip(normalized_digest.as_deref())
        .map(|(value, digest)| (value.len(), digest));
    let injected = normalized_identity.is_some_and(|identity| {
        identity == PLAYWRIGHT_1_61_INJECTED || identity == PLAYWRIGHT_1_62_INJECTED
    });
    let pinned = is_pinned_bootstrap_identity(identity.0, identity.1) || injected;
    if params
        .get("contextId")
        .and_then(Value::as_u64)
        .is_none_or(|id| id == 0 || id > 1_000_000)
        || expression.len() > crate::MAX_FRAME_BYTES
        || !pinned
    {
        return Err(CdpError::new(
            CdpErrorCode::InvalidParams,
            "unrecognized bounded runtime bootstrap",
        ));
    }
    let (class_name, object_id) = if injected {
        ("InjectedScript", "playwright-injected-script")
    } else {
        ("UtilityScript", "playwright-utility-script")
    };
    Ok(
        json!({"result":{"type":"object","subtype":"object","className":class_name,"description":class_name,"objectId":object_id}}),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        bootstrap_injected_script, is_pinned_bootstrap_identity, normalize_playwright_frame_seq,
    };

    #[test]
    fn normalizes_one_bounded_playwright_frame_sequence() {
        assert_eq!(
            normalize_playwright_frame_seq(r#"prefix{"frameSeq":27,"option":true}suffix"#),
            Some(r#"prefix{"frameSeq":0,"option":true}suffix"#.to_owned())
        );
    }

    #[test]
    fn rejects_ambiguous_or_unbounded_playwright_frame_sequences() {
        assert_eq!(
            normalize_playwright_frame_seq(r#"{"frameSeq":1,"frameSeq":2}"#),
            None
        );
        assert_eq!(
            normalize_playwright_frame_seq(r#"{"frameSeq":4294967296}"#),
            None
        );
        assert_eq!(normalize_playwright_frame_seq(r#"{"frameSeq":-1}"#), None);
    }

    #[test]
    fn accepts_playwright_1_62_injected_bootstrap_identity() {
        assert!(is_pinned_bootstrap_identity(
            316_703,
            "60769c02b1d2f781a31f54e504ad2ee5fdf397b4fe47b43e7284f6d4e231a771"
        ));
    }

    #[test]
    fn accepts_playwright_1_62_utility_bootstrap_identity() {
        assert!(is_pinned_bootstrap_identity(
            11_035,
            "fd3b882cab3a898b34e827ab330d9fb152340d61f7d0c3cec30247018af131e4"
        ));
    }

    #[test]
    fn rejects_substring_lookalike_bootstrap() {
        let result = bootstrap_injected_script(&json!({
            "expression": "new (module.exports.InjectedScript()) browserName\":\"chromium",
            "contextId": 1
        }));
        assert!(result.is_err());
    }

    #[test]
    fn rejects_mutated_pinned_bootstrap() {
        let result = bootstrap_injected_script(&json!({
            "expression": "new (module.exports.UtilityScript())/*mutated*/",
            "contextId": 1
        }));
        assert!(result.is_err());
    }
}
