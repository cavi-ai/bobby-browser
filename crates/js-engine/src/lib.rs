//! Bounded, synchronous shaping of JavaScript evaluation results.
//!
//! This crate has no I/O and no async surface: it exists to keep result-size
//! policy (how big a `JavaScriptResult` evidence payload is allowed to be) out
//! of the worker implementations that produce the raw value.

/// Serialize `value` and, if it fits within `max_bytes`, return it unchanged
/// with `truncated = false`.
///
/// If the serialized form exceeds `max_bytes` (or `value` cannot be
/// serialized at all), returns a `serde_json::Value::String` truncation
/// marker containing the first `max_bytes` characters of the serialized form
/// (or a generic marker on serialization failure) with `truncated = true`.
///
/// Pure and synchronous: no I/O, no allocation beyond what serialization
/// itself requires.
pub fn bound_result(value: serde_json::Value, max_bytes: usize) -> (serde_json::Value, bool) {
    match serde_json::to_string(&value) {
        Ok(serialized) if serialized.len() <= max_bytes => (value, false),
        Ok(serialized) => (truncated_marker(&serialized, max_bytes), true),
        Err(_) => (
            serde_json::Value::String("[unserializable result]".to_string()),
            true,
        ),
    }
}

fn truncated_marker(serialized: &str, max_bytes: usize) -> serde_json::Value {
    // `max_bytes` is a char budget here, not a byte-exact cutoff: we take a
    // prefix on a char boundary so multi-byte UTF-8 sequences are never
    // split, then flag the result as truncated regardless.
    let prefix: String = serialized.chars().take(max_bytes).collect();
    serde_json::Value::String(format!("{prefix}…[truncated]"))
}

#[cfg(test)]
mod tests {
    use super::bound_result;
    use serde_json::json;

    #[test]
    fn small_value_passes_through_untruncated() {
        let value = json!({"ok": true, "n": 42});
        let (result, truncated) = bound_result(value.clone(), 1024);
        assert_eq!(result, value);
        assert!(!truncated);
    }

    #[test]
    fn oversize_value_is_truncated_and_flagged() {
        let value = json!("x".repeat(1000));
        let (result, truncated) = bound_result(value, 16);
        assert!(truncated);
        let serde_json::Value::String(marker) = result else {
            panic!("expected a string truncation marker");
        };
        assert!(marker.ends_with("…[truncated]"));
        assert!(marker.len() < 1000);
    }

    #[test]
    fn boundary_exactly_at_max_is_not_truncated() {
        // `serde_json::to_string(&json!("aa"))` serializes to `"aa"` (4 bytes
        // including quotes) — pick max_bytes to match exactly.
        let value = json!("aa");
        let serialized_len = serde_json::to_string(&value).unwrap().len();
        let (result, truncated) = bound_result(value.clone(), serialized_len);
        assert_eq!(result, value);
        assert!(!truncated);
    }

    #[test]
    fn one_byte_over_boundary_truncates() {
        let value = json!("aaa");
        let serialized_len = serde_json::to_string(&value).unwrap().len();
        let (result, truncated) = bound_result(value, serialized_len - 1);
        assert!(truncated);
        assert!(matches!(result, serde_json::Value::String(_)));
    }
}
