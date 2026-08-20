//! Machine-readable repair hints attached to failures at the MCP boundary.
//!
//! `bobby://failure-taxonomy` stays the source of truth in prose; this map
//! distills each code's general repair into one sentence so an agent can act
//! without reading the resource first. The taxonomy's own rule stands: where
//! a tool description gives a more precise repair, the tool description wins.

use serde_json::{json, Value};

const TAXONOMY_DOC: &str = "bobby://failure-taxonomy";

/// The `needsReconciliation` override: the outcome is not a plain failure and
/// retrying can double-apply a side effect, so its repair always wins over
/// whatever the carried error code would otherwise say.
const NEEDS_RECONCILIATION_ACTION: &str =
    "Do not retry. Call recovery_status for the workflow, then workflow_recover if a checkpoint exists.";

fn repair(action: &str) -> Value {
    json!({"action": action, "doc": TAXONOMY_DOC})
}

/// Repair for a `needsReconciliation` outcome, regardless of error code.
pub(crate) fn reconciliation_repair() -> Value {
    repair(NEEDS_RECONCILIATION_ACTION)
}

/// Repair for a navigation Chrome aborted (a download response or a cancelled
/// navigation): the generic `browserCommandFailed` advice says retry, and a
/// retry repeats the abort.
pub(crate) fn navigation_aborted_repair() -> Value {
    repair("Do not retry the navigation; fetch files with download_url, or use click_and_wait_for_download for an in-page link, and navigate only to renderable URLs.")
}

pub(crate) fn candidate_limit_repair() -> Value {
    repair("Narrow the target using role + accessibleName, label, testId, CSS, or ordinal, then retry once; the error lists the first bounded matches and the exact count/limit.")
}

pub(crate) fn browser_launch_repair() -> Value {
    json!({
        "action": "run `bobby doctor`; another runtime (bobby serve/cdp/mcp-stdio or a stray mcp-gateway) may own the Firefox companion port (default 127.0.0.1:9876) or the BiDi endpoint; stop it or point this runtime at a free companionBind, then retry session_create"
    })
}

/// General repair for one `ErrorCode` or `InterfaceErrorCode` wire name
/// (both serialize camelCase from the same vocabulary). Unknown codes get no
/// hint rather than a guessed one.
pub(crate) fn repair_for_code(code: &str) -> Option<Value> {
    let action = match code {
        "invalidRequest" => "Fix the named argument and resubmit; nothing ran.",
        "notFound" => "Re-list with page_list or session_list and use a current id.",
        "deadlineExceeded" => {
            "Confirm the condition is reachable, then retry with a longer timeout or deadline."
        }
        "browserLaunchFailed" => {
            "Environment problem, not a bad call; retry session_create and escalate if it persists."
        }
        "browserCommandFailed" => {
            "Retry the same call; recreate the session or page if it keeps failing."
        }
        "verificationFailed" => {
            "Read the returned validation detail, correct the specific failure, and retry only that step; do not blind-retry."
        }
        "journalFailed" => {
            "Resubmit with the same idempotency key; if the status is needsReconciliation, call recovery_status first and never retry the command."
        }
        "resourceExhausted" => {
            "Free capacity first (session_list, then session_close or close an idle page), then retry."
        }
        "policyDenied" => "Not retryable as-is; use an allowed path or policy, or a different tool.",
        "internal" => "Nothing caller-side to fix; treat as non-retryable and escalate if it recurs.",
        "targetNotFound" => {
            "Take a fresh a11y_snapshot (form_snapshot for typed controls) and pass the new target."
        }
        "targetAmbiguous" => {
            "Narrow the purpose or hints until exactly one candidate matches, or allow best-match resolution."
        }
        "frameNotFound" => "Re-resolve the target from a fresh snapshot.",
        "shadowRootUnavailable" => {
            "Re-resolve the target; the shadow root may not be attached yet."
        }
        "targetDetached" => "Re-resolve the target; the page changed underneath the call.",
        "targetObscured" => {
            "Clear what is on top (for example intent_dismiss_obstruction) or scroll the element into the clear, then retry."
        }
        "targetOutOfBounds" => {
            "Bring the element into view (scroll, resize, or emulate a larger viewport), then retry."
        }
        "waitConditionTimedOut" => {
            "Confirm the condition via inspect, then retry with a longer timeout."
        }
        "expectedStatePreSatisfied" => {
            "The expected state held before the act ran, so passing proves nothing. Strengthen expectedState to content that only appears after the act (a confirmation id, status change, or new element), then resubmit."
        }
        "screenshotCaptureFailed" => {
            "Retry; if it persists, the page, engine, or artifact store may be in a bad state."
        }
        "networkPolicyDenied" => "The URL failed network policy: non-http(s) scheme, embedded credentials, or a denied destination. Loopback and private addresses are denied unless the operator sets http.allow_loopback / http.allow_private_network in config. For a file the page already offers, prefer clicking its download link over download_url.",
        "httpResponseTooLarge" => {
            "Raise the byte limit within the configured range, or expect a smaller resource."
        }
        "httpTransferFailed" => "Retryable; resubmit the same call.",
        "httpStateConflict" => {
            "Not retryable as the same attempt; issue a fresh call so it re-snapshots current state."
        }
        "httpEquivalenceUnproven" => {
            "Not retryable as-is; bring the page to a state where equivalence can be proven."
        }
        "intentCompileFailed" => {
            "Fix the request shape (purpose, field list, field names) and resubmit; nothing was attempted."
        }
        "intentActionMismatch" => {
            "Re-check the control's real role or kind and match the action to it."
        }
        "obstructionSuspected" => {
            "Take a fresh a11y_snapshot; there may be another dismissal control, or the wrong thing was dismissed."
        }
        "visionAssistDenied" => {
            "The message leads with the deterministic stuck reason (targetNotFound, targetAmbiguous, obstructionSuspected); repair that first (fresh a11y_snapshot, narrower target); enabling visionAssist on the session or granting vision:assist only adds the vision fallback."
        }
        "visionAssistFailed" => {
            "If no vision provider is configured, treat like visionAssistDenied; only transient causes (capture error, response error, low-confidence proposal) merit a single retry."
        }
        // RPC-layer codes (InterfaceErrorCode) with no command-layer twin.
        "authenticationFailed" => {
            "Re-source the credential (bootstrap.env or AUTOMATION_RUNTIME_BOOTSTRAP_*); do not retry with the same token."
        }
        "tokenExpired" => {
            "Run bobby init --force, update the host environment, then reconnect."
        }
        "missingCapability" => {
            "Re-issue the credential with the requiredCapability named in the error, or pick a tool the current grant covers."
        }
        "idempotencyConflict" => {
            "Mint a fresh idempotency key; never reuse a key across different calls."
        }
        "invalidIdempotencyKey" => "Send a well-formed key, or omit the field; the call had no effect.",
        "malformedScope" => "Re-read the ids from session_list / page_list.",
        "artifactDenied" => "Re-capture the artifact with a command this principal owns.",
        "unsupportedInterfaceVersion" => {
            "Match the client's interface version to the one runtime_info advertises."
        }
        "unsupportedOperation" => "Check the tool name against tools/list.",
        "engineUnreachable" => {
            "The configured browser engine did not answer; the call itself was fine. Run `bobby doctor`, start or re-point the engine it names, then resubmit unchanged."
        }
        _ => return None,
    };
    Some(repair(action))
}

/// Migration mapping appended to a schema-violation repair when the rejected
/// payload still carries a pre-0.11.0 `FillValue` marker: the wire vocabulary
/// unified onto `ControlAction`'s `kind`+field spelling (`crates/types/tests/
/// contracts.rs`, "L3 unification contract").
const LEGACY_FILL_SHAPE_MIGRATION: &str = "The payload also still uses the legacy fill shape, \
    changed in 0.11.0: kind \"text\" (field \"text\") is now kind \"setText\" (field \"value\"); \
    kind \"select\" (field \"option\") is now kind \"selectOne\" (field \"value\"); \
    kind \"checked\" is now kind \"setChecked\"; kind \"files\" is now kind \"setFiles\".";

/// Tools whose arguments can carry a fill or control-action value, and so are
/// worth scanning for the legacy `FillValue` shape on rejection.
const FILL_SHAPE_TOOLS: &[&str] = &[
    "intent_fill",
    "intent_complete_form",
    "control_action",
    "command_execute",
];

/// True when `value`, or anything nested inside it, still carries a
/// pre-0.11.0 `FillValue` marker. None of these spellings exist in the
/// unified `ControlAction` vocabulary, so seeing one means the caller has not
/// migrated yet rather than having sent a differently-broken payload.
fn contains_legacy_fill_shape(value: &Value) -> bool {
    match value {
        Value::Object(fields) => {
            let kind = fields.get("kind").and_then(Value::as_str);
            (kind == Some("text") && fields.contains_key("text"))
                || fields.contains_key("option")
                || matches!(kind, Some("select") | Some("checked") | Some("files"))
                || fields.values().any(contains_legacy_fill_shape)
        }
        Value::Array(items) => items.iter().any(contains_legacy_fill_shape),
        _ => false,
    }
}

/// The legacy-shape migration text for a schema-violation on `tool`, when its
/// rejected `arguments` still carry a pre-0.11.0 `FillValue` marker.
pub(crate) fn legacy_fill_shape_migration(tool: &str, arguments: &Value) -> Option<&'static str> {
    (FILL_SHAPE_TOOLS.contains(&tool) && contains_legacy_fill_shape(arguments))
        .then_some(LEGACY_FILL_SHAPE_MIGRATION)
}

/// Repair for a protocol-layer `-32602` rejection reason string.
pub(crate) fn repair_for_protocol_reason(reason: &str) -> Option<Value> {
    let action = match reason {
        "schemaViolation" => {
            "Fix the value at error.data.pointer; error.data.constraint names the keyword it violated."
        }
        "malformedArguments" => {
            "Re-read the tool's inputSchema and description; a bound checked outside the schema failed."
        }
        "deadlineOutOfRange" => {
            "Set the envelope's deadline within the allowed window (not past, not over 300,000 ms out) and resubmit."
        }
        "invalidIdempotencyKey" => "Send a well-formed key, or omit the field; the call had no effect.",
        "workflowBindingConflict" => {
            "Use the workflowHandle alone for page work, or omit it and send the complete explicit ID set."
        }
        "unknownWorkflowHandle" => {
            "The handle is malformed, unknown, or evicted; use explicit IDs to inspect or close the workflow's resources, then call workflow_start for a new handle."
        }
        _ => return None,
    };
    Some(repair(action))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every advertised `ErrorCode` must have a repair hint; adding a code
    /// without one fails here, not in front of an agent.
    #[test]
    fn every_advertised_error_code_has_a_repair() {
        let codes = crate::schema::error_code_for_test();
        for code in codes["enum"].as_array().expect("error code enum") {
            let code = code.as_str().expect("code is a string");
            assert!(repair_for_code(code).is_some(), "no repair for {code}");
        }
    }

    /// The RPC-layer codes with no command-layer twin
    /// (`types::InterfaceErrorCode`, camelCase on the wire).
    #[test]
    fn every_rpc_layer_code_has_a_repair() {
        for code in [
            "authenticationFailed",
            "tokenExpired",
            "missingCapability",
            "idempotencyConflict",
            "invalidIdempotencyKey",
            "malformedScope",
            "artifactDenied",
            "unsupportedInterfaceVersion",
            "unsupportedOperation",
            "engineUnreachable",
        ] {
            assert!(repair_for_code(code).is_some(), "no repair for {code}");
        }
    }

    #[test]
    fn unknown_codes_get_no_guessed_hint() {
        assert!(repair_for_code("madeUpCode").is_none());
    }

    #[test]
    fn aborted_navigation_repair_points_at_the_download_tools() {
        let action = navigation_aborted_repair()["action"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(action.contains("download_url"), "{action}");
        assert!(action.starts_with("Do not retry"), "{action}");
    }

    #[test]
    fn reconciliation_repair_forbids_retry() {
        let hint = reconciliation_repair();
        assert!(hint["action"].as_str().unwrap().contains("Do not retry"));
        assert_eq!(hint["doc"], json!(TAXONOMY_DOC));
    }
}
