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

/// Recognizes Playwright's pinned injected-script bootstrap without executing caller JavaScript.
pub(crate) fn bootstrap_injected_script(params: &Value) -> Result<Value, CdpError> {
    let expression = params
        .get("expression")
        .and_then(Value::as_str)
        .ok_or_else(|| CdpError::new(CdpErrorCode::InvalidParams, "missing runtime expression"))?;
    let digest = format!("{:x}", Sha256::digest(expression.as_bytes()));
    let identity = (expression.len(), digest.as_str());
    let injected = identity == PLAYWRIGHT_1_61_INJECTED || identity == PLAYWRIGHT_1_62_INJECTED;
    if params
        .get("contextId")
        .and_then(Value::as_u64)
        .is_none_or(|id| id == 0 || id > 1_000_000)
        || expression.len() > crate::MAX_FRAME_BYTES
        || !is_pinned_bootstrap_identity(expression.len(), &digest)
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

    use super::{bootstrap_injected_script, is_pinned_bootstrap_identity};

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
