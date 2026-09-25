//! Machine-readable repair hints attached to failures at the MCP boundary.
//!
//! `bobby://failure-taxonomy` stays the source of truth in prose; this map
//! distills each code's general repair into one sentence so an agent can act
//! without reading the resource first. The taxonomy's own rule stands: where
//! a tool description gives a more precise repair, the tool description wins.

use serde_json::{json, Value};

use crate::protocol::{
    INTERFACE_ERROR, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND, NOT_INITIALIZED,
    PARSE_ERROR, REQUEST_CANCELLED,
};

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

/// Repair for a mutating call that failed because its workflow handle was
/// following a popup that has since closed. The handle is already rebound
/// to `opener_page_id` by the time this repair is read -- never retry the
/// call as-is, since it would re-target a page that is now gone.
pub(crate) fn popup_closed_repair(opener_page_id: &str) -> Value {
    repair(&format!(
        "The popup this workflow handle was following closed; the handle is now bound to page {opener_page_id}. Do not retry this call unchanged -- act on the opener (a read-only call through the same handle replays there automatically), or pass pageId explicitly if you meant the opener all along."
    ))
}

/// Repair for a fill or select intent (`intent_fill`, `intent_complete_form`)
/// that resolved to a file control (`ErrorCode::IntentActionMismatch`,
/// `crates/intent-engine/src/engine.rs`'s `file_control_failure`). The
/// generic `intentActionMismatch` advice ("re-check the role") does not
/// apply here: no role or kind adjustment makes a fill act on a file input,
/// only a different tool. The controlId itself is per-instance and already
/// in the message, not a canned string this repair can carry.
pub(crate) fn file_control_repair() -> Value {
    repair("Do not retry a fill or select intent on this target; call upload_files (or control_action with a setFiles action) using the controlId from workflow_observe (includeForms:true) or form_snapshot.")
}

pub(crate) fn candidate_limit_repair() -> Value {
    repair("Narrow the target using role + accessibleName, label, testId, CSS, or ordinal, then retry once; the error lists the first bounded matches and the exact count/limit.")
}

pub(crate) fn browser_launch_repair() -> Value {
    repair("Environment problem, not a bad argument: run `bobby doctor`; another runtime (bobby serve/cdp/mcp-stdio or a stray mcp-gateway) may own the Firefox companion port (default 127.0.0.1:9876) or the BiDi endpoint. Stop it, start and Pair the companion if needed, or point this runtime at a free companionBind, then retry session_create.")
}

pub(crate) fn duplicate_request_id_repair() -> Value {
    repair("Use a unique id per request; wait for the earlier response or send notifications/cancelled for it first.")
}

pub(crate) fn frame_too_large_repair() -> Value {
    repair("Keep each request frame under maxBytes; pass large inputs by reference (a file path or URL), not inline.")
}

/// The call ran; only its result was dropped for size, so resubmitting a
/// mutating call would apply its effect a second time.
pub(crate) fn result_too_large_repair() -> Value {
    repair("The call ran but its result exceeded maxBytes and was dropped. Do not resubmit a mutating call; read its outcome with recovery_status. For a read, ask for less (a smaller limit, evidenceDetail compact, or a clipped screenshot).")
}

pub(crate) fn event_gap_repair() -> Value {
    repair("Resume events_read from eventGap.earliestAvailable; the events before it are gone, so re-read current state (recovery_status or workflow_observe) instead of replaying them.")
}

pub(crate) fn artifact_not_found_repair() -> Value {
    repair("Call resources/list for the artifact:// URIs this principal can read; the artifact may have been evicted or captured by another principal.")
}

pub(crate) fn resource_too_large_repair() -> Value {
    repair("Not retryable as-is: the artifact exceeds maxEncodedBytes for resources/read. Capture a smaller one (a clipped screenshot) or fetch it with GET /v1/artifacts/{id} from bobby serve.")
}

pub(crate) fn job_not_found_repair() -> Value {
    repair("Use a jobId returned by job_submit; a job is visible only to the principal that submitted it.")
}

/// Repair for a JSON-RPC error `code` whose site attached no more specific
/// one. `protocol::error` puts it on `error.data.repair` and, because hosts
/// render only `error.message`, on the message too.
pub(crate) fn repair_for_rpc_code(code: i64) -> Value {
    let action = match code {
        PARSE_ERROR => "Send each message as one complete JSON value, one per line on stdio; nothing ran.",
        INVALID_REQUEST => {
            "Send one JSON-RPC 2.0 object carrying only jsonrpc \"2.0\", a string or number id, method, and params; nothing ran."
        }
        METHOD_NOT_FOUND => {
            "Check the name against tools/list or the MCP methods this server implements; a tool the credential's capabilities do not cover, or a job tool on a runtime without jobs, is not callable."
        }
        INVALID_PARAMS => {
            "Re-read what the method takes (for tools/call, the tool's inputSchema in tools/list; for prompts/get and resources/read, a name or uri from prompts/list or resources/list) and resubmit; nothing ran."
        }
        NOT_INITIALIZED => {
            "Send initialize, then the notifications/initialized notification, before any other request."
        }
        REQUEST_CANCELLED => {
            "The request was cancelled before a result returned; if it could have acted on the page, check recovery_status before resubmitting."
        }
        INTERFACE_ERROR => {
            "Read the interface error code in error.data and follow its entry in bobby://failure-taxonomy."
        }
        _ => INTERNAL_ACTION,
    };
    repair(action)
}

const INTERNAL_ACTION: &str =
    "Nothing caller-side to fix; treat as non-retryable and escalate if it recurs.";

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
        "internal" => INTERNAL_ACTION,
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
        "boundaryAlreadyExecuted" => {
            "A Boundary submit for this workflow already completed and its effect is on record (the error names the prior commandId). Inspect that outcome's evidence or recovery_status instead of resubmitting; pass reSubmit: true only when a second effect is genuinely intended."
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
        "hintsPerField" => {
            "intent_complete_form takes hints per field (fields[].hints); a top-level hints applies only when fields has exactly one entry."
        }
        "controlIdNotFound" => {
            "Take a fresh form_snapshot (or workflow_observe with includeForms:true) and pass a controlId from it; a controlId does not survive a page change."
        }
        "exactlyOneOfWorkflowIdOrSessionId" => {
            "Pass exactly one of workflowId or sessionId, not both and not neither."
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

    /// Each JSON-RPC code the gateway emits has its own repair; only
    /// `-32603` and codes it never emits share the internal fallback.
    #[test]
    fn every_emitted_rpc_code_has_its_own_repair() {
        let fallback = repair_for_rpc_code(i64::MIN);
        let mut actions = std::collections::BTreeSet::new();
        for code in [
            PARSE_ERROR,
            INVALID_REQUEST,
            METHOD_NOT_FOUND,
            INVALID_PARAMS,
            NOT_INITIALIZED,
            REQUEST_CANCELLED,
            INTERFACE_ERROR,
        ] {
            let hint = repair_for_rpc_code(code);
            assert_ne!(
                hint, fallback,
                "code {code} fell through to the internal repair"
            );
            assert_eq!(hint["doc"], json!(TAXONOMY_DOC));
            assert!(actions.insert(hint["action"].as_str().unwrap().to_owned()));
        }
        assert_eq!(
            repair_for_rpc_code(crate::protocol::INTERNAL_ERROR),
            repair_for_code("internal").unwrap()
        );
    }

    /// A cancelled call may already have acted; its repair must send the
    /// agent to recovery_status, not straight back to a resubmit.
    #[test]
    fn cancelled_and_oversized_result_repairs_never_invite_a_blind_resubmit() {
        let cancelled = repair_for_rpc_code(REQUEST_CANCELLED);
        assert!(cancelled["action"]
            .as_str()
            .unwrap()
            .contains("recovery_status"));
        let dropped = result_too_large_repair();
        let action = dropped["action"].as_str().unwrap();
        assert!(
            action.contains("Do not resubmit a mutating call"),
            "{action}"
        );
        assert!(action.contains("recovery_status"), "{action}");
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
    fn file_control_repair_points_at_upload_files_not_a_role_fix() {
        let action = file_control_repair()["action"].as_str().unwrap().to_owned();
        assert!(action.contains("upload_files"), "{action}");
        assert!(action.starts_with("Do not retry"), "{action}");
        // The generic intentActionMismatch repair below is the wrong advice
        // for a file control; this one must not repeat it.
        assert!(
            !action.contains("Re-check the control's real role"),
            "{action}"
        );
    }

    #[test]
    fn hints_per_field_repair_names_the_field_scoped_shape() {
        let action = repair_for_protocol_reason("hintsPerField")
            .expect("hintsPerField has a repair")["action"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(action.contains("fields[].hints"), "{action}");
        assert!(action.contains("exactly one entry"), "{action}");
    }

    #[test]
    fn reconciliation_repair_forbids_retry() {
        let hint = reconciliation_repair();
        assert!(hint["action"].as_str().unwrap().contains("Do not retry"));
        assert_eq!(hint["doc"], json!(TAXONOMY_DOC));
    }
}
