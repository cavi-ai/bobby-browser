use serde_json::{json, Value};

use crate::{CdpError, CdpErrorCode};

/// Recognizes Playwright's pinned injected-script bootstrap without executing caller JavaScript.
pub(crate) fn bootstrap_injected_script(params: &Value) -> Result<Value, CdpError> {
    let expression = params
        .get("expression")
        .and_then(Value::as_str)
        .ok_or_else(|| CdpError::new(CdpErrorCode::InvalidParams, "missing runtime expression"))?;
    let injected = expression.contains("new (module.exports.InjectedScript())")
        && expression.contains("browserName\":\"chromium");
    let utility = expression.contains("new (module.exports.UtilityScript())");
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
