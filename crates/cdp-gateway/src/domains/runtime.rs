use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{CdpError, CdpErrorCode};

/// Recognizes Playwright's pinned injected-script bootstrap without executing caller JavaScript.
pub(crate) fn bootstrap_injected_script(params: &Value) -> Result<Value, CdpError> {
    let expression = params
        .get("expression")
        .and_then(Value::as_str)
        .ok_or_else(|| CdpError::new(CdpErrorCode::InvalidParams, "missing runtime expression"))?;
    const INJECTED_SHA256: &str =
        "219b161932480469c7ebe3baf2d66a8101276625916b6a60ad4e57f4858eb6cc";
    const UTILITY_SHA256: &str = "3fc2ec24a4359c88a30f650d4daf23c0554eda935fb19bf1fe81f687f65d8dcd";
    let digest = format!("{:x}", Sha256::digest(expression.as_bytes()));
    let injected = expression.len() == 311_362 && digest == INJECTED_SHA256;
    let utility = expression.len() == 10_652 && digest == UTILITY_SHA256;
    if params
        .get("contextId")
        .and_then(Value::as_u64)
        .is_none_or(|id| id == 0 || id > 1_000_000)
        || expression.len() > crate::MAX_FRAME_BYTES
        || !(injected || utility)
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

    use super::bootstrap_injected_script;

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
