use std::collections::BTreeSet;

use serde_json::{json, Map, Value};

use crate::protocol::MAX_EVENT_LIMIT;
use crate::workflow_handles::{workflow_scope_for_tool, WorkflowScope};

pub(crate) const MAX_EVIDENCE_ITEMS: usize = 128;
const MAX_COLLECTION_ITEMS: usize = 128;
const MAX_STRING_BYTES: usize = 4096;
const MAX_URL_BYTES: usize = 8192;
const MAX_HTML_BYTES: usize = 64 * 1024;
const MAX_TIMEOUT_MS: u64 = 300_000;
const MAX_NETWORK_IGNORE_SUBSTRINGS: usize = 32;
const MAX_NETWORK_IGNORE_SUBSTRING_BYTES: usize = 512;
const MAX_NETWORK_IGNORE_RESOURCE_TYPES: usize = 32;
const MAX_EXCLUDED_CLASSES: usize = 64;
pub(crate) const MAX_WORKFLOW_GOAL_SCALARS: usize = 256;
// Output-only: bounds for definitions reachable solely from `structuredContent`,
// never from a tool argument.
const MAX_RECOVERY_RECEIPTS: usize = 64;
/// Cap on workflows a single `recovery_status` discovery answers with.
pub(crate) const MAX_RECOVERABLE_WORKFLOWS: usize = 32;

pub(crate) fn validate_tool_arguments(
    name: &str,
    arguments: &Value,
) -> Result<(), SchemaViolation> {
    let schema = tool_schema(name);
    validate(&schema, &schema, arguments)
}

pub(crate) fn tool_schema(name: &str) -> Value {
    let (properties, required) = match name {
        "runtime_info" | "session_list" => (json!({}), vec![]),
        "session_create" => (
            json!({
                "profile": string(1, 128),
                "proxy": nullable(string(0, 2048)),
                "executionPolicy": object(
                    json!({
                        "javascriptEvaluation":{"type":"boolean"},
                        "visionAssist":{"type":"boolean"},
                        "fingerprint":{"type":"boolean"},
                        "humanize":{"type":"boolean"},
                        "visionNode":string(1, 128)
                    }),
                    &[]
                ),
                "zigzagzig":{"type":"boolean"}
            }),
            vec!["profile"],
        ),
        "workflow_start" => (
            json!({
                "profile": string(1, 128),
                "proxy": nullable(string(0, 2048)),
                "executionPolicy": object(
                    json!({
                        "javascriptEvaluation":{"type":"boolean"},
                        "visionAssist":{"type":"boolean"},
                        "fingerprint":{"type":"boolean"},
                        "humanize":{"type":"boolean"},
                        "visionNode":string(1, 128)
                    }),
                    &[]
                ),
                "zigzagzig":{"type":"boolean"},
                "url": string(1, MAX_URL_BYTES)
            }),
            vec!["profile"],
        ),
        "workflow_observe" => (
            json!({
                "workflowHandle": workflow_handle(),
                // `validate_string` is intentionally byte-oriented. Four bytes per
                // scalar prevents it from rejecting a valid 256-scalar UTF-8 goal;
                // the observe parser applies the scalar limit before registry lookup.
                "goal": string(0, MAX_WORKFLOW_GOAL_SCALARS * 4),
                "maxNodes": {"type":"integer","minimum":1,"maximum":2048},
                "includeForms": {"type":"boolean"},
                "maxControls": {"type":"integer","minimum":1,"maximum":512},
                "evidenceDetail":{"type":"string","enum":["compact","full"]},
                "target": {"type":"object","description":"Scope the observation to one region's subtree (same shape as a11y_snapshot.target) instead of the whole page"}
            }),
            vec!["workflowHandle"],
        ),
        "context_ask" => (
            json!({
                "sessionId": id(),
                "pageId": id(),
                "description": string(1, 256)
            }),
            vec!["sessionId", "pageId", "description"],
        ),
        "context_neighbors" => (
            json!({
                "sessionId": id(),
                "pageId": id(),
                "description": string(1, 256)
            }),
            vec!["sessionId", "pageId", "description"],
        ),
        "toolset_select" => (
            json!({
                "toolset": {"type":"string","enum":["full","explore","act","intent","verify"]}
            }),
            vec!["toolset"],
        ),
        "page_open" => (
            json!({"sessionId": id(), "url": string(1, MAX_URL_BYTES)}),
            vec!["sessionId"],
        ),
        "page_close" => (
            json!({"sessionId": id(), "pageId": id(), "workflowId": id()}),
            vec!["sessionId", "pageId"],
        ),
        "page_activate" => (
            json!({"sessionId": id(), "pageId": id(), "workflowId": id()}),
            vec!["sessionId", "pageId"],
        ),
        "a11y_snapshot" => (
            json!({
                "workflowId": id(),
                "sessionId": id(),
                "pageId": id(),
                "maxNodes": {"type":"integer","minimum":1,"maximum":2048},
                "target": {"type":"object","description":"Scope the tree to this target's subtree (e.g. the form being worked on) instead of the whole page; same shape as wait_for targets"}
            }),
            vec!["sessionId", "pageId"],
        ),
        "form_snapshot" => (
            json!({"workflowId": id(), "sessionId": id(), "pageId": id(), "maxControls":{"type":"integer","minimum":1,"maximum":512}}),
            vec!["sessionId", "pageId"],
        ),
        "control_action" => (
            json!({
                "workflowId": id(),
                "sessionId": id(),
                "pageId": id(),
                "target": {
                    "type":"object",
                    "additionalProperties":false,
                    "properties":{
                        "role":string(1,128),
                        "accessibleName":string(1,2048),
                        "ordinal":{"type":["integer","null"],"minimum":0,"maximum":2047},
                        "framePath":array(any_value(),8),
                        "shadowPath":array(any_value(),8)
                    },
                    "required":["role","accessibleName"]
                },
                "action": {"oneOf":[
                    {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"setText"},"value":string(0,4096),"clearFirst":{"type":"boolean","default":true,"description":"replace the current value; set false to append"}},"required":["kind","value"]},
                    {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"setChecked"},"checked":{"type":"boolean"}},"required":["kind","checked"]},
                    {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"selectOne"},"value":string(0,4096)},"required":["kind","value"]},
                    {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"selectMany"},"values":nonempty_array(string(0,4096),512)},"required":["kind","values"]},
                    {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"setFiles"},"paths":nonempty_array(string(1,4096),512)},"required":["kind","paths"]},
                    {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"clear"}},"required":["kind"]},
                    {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"activate"}},"required":["kind"]}
                ]}
            }),
            vec!["sessionId", "pageId", "target", "action"],
        ),
        "network_log" => (
            json!({
                "workflowId": id(),
                "sessionId": id(),
                "pageId": id(),
                "clear": {"type":"boolean"}
            }),
            vec!["sessionId", "pageId"],
        ),
        "emulate" => (
            json!({
                "workflowId": id(),
                "sessionId": id(),
                "pageId": id(),
                "viewport": nullable(object(
                    json!({"width":{"type":"integer","minimum":1,"maximum":16384},"height":{"type":"integer","minimum":1,"maximum":16384}}),
                    &["width", "height"]
                )),
                "geolocation": nullable(object(
                    json!({
                        "latitude":{"type":"number","minimum":-90,"maximum":90},
                        "longitude":{"type":"number","minimum":-180,"maximum":180},
                        "accuracy":nullable(json!({"type":"number","minimum":0}))
                    }),
                    &["latitude", "longitude"]
                )),
                "mobile": {"type":"boolean"}
            }),
            vec!["sessionId", "pageId"],
        ),
        "dialog" => (
            json!({
                "workflowId": id(),
                "sessionId": id(),
                "pageId": id(),
                "action": {"type":"string","enum":["accept","dismiss"]},
                "timeoutMs": timeout_ms()
            }),
            vec!["sessionId", "pageId", "action"],
        ),
        "pdf" => (
            json!({
                "workflowId": id(),
                "sessionId": id(),
                "pageId": id(),
                "landscape": {"type":"boolean"},
                "printBackground": {"type":"boolean"},
                "scale": {"type":"number","minimum":0.1,"maximum":2.0},
                "pageRanges": nullable(string(0, 256))
            }),
            vec!["sessionId", "pageId"],
        ),
        "cookie_get" => (
            json!({
                "workflowId": id(),
                "sessionId": id(),
                "pageId": id(),
                "urls": array(string(1, MAX_URL_BYTES), 64)
            }),
            vec!["sessionId", "pageId"],
        ),
        "cookie_set" => (
            json!({
                "workflowId": id(),
                "sessionId": id(),
                "pageId": id(),
                "cookies": array(json!({"$ref":"#/$defs/SetCookieParam"}), 128)
            }),
            vec!["sessionId", "pageId", "cookies"],
        ),
        "cookie_delete" => (
            json!({
                "workflowId": id(),
                "sessionId": id(),
                "pageId": id(),
                "urls": array(string(1, MAX_URL_BYTES), 64),
                "names": array(string(1, 1024), 128)
            }),
            vec!["sessionId", "pageId"],
        ),
        "extract_structured" => (
            json!({
                "workflowId": id(),
                "sessionId": id(),
                "pageId": id(),
                "schema": any_value(),
                "purpose": nullable(string(1, 256))
            }),
            vec!["sessionId", "pageId", "schema"],
        ),
        "session_close" => (json!({"sessionId": id()}), vec!["sessionId"]),
        "navigate" => (
            json!({
                "workflowId": id(),
                "sessionId": id(),
                "pageId": id(),
                "url": string(1, MAX_URL_BYTES),
                "waitUntil": {"type":"string","enum":["commit","domContentLoaded","interactive","networkIdle"]},
                "timeoutMs": timeout_ms()
            }),
            vec!["sessionId", "pageId", "url"],
        ),
        "click" => (
            json!({
                "workflowId": id(),
                "commandId": id_pin(),
                "attemptId": id_pin(),
                "sessionId": id(),
                "pageId": id(),
                "selector": string(1, MAX_STRING_BYTES),
                "target": nullable(json!({"$ref":"#/$defs/TargetSpec"})),
                "boundary": {"type":"boolean"},
                "autoCheckpoint":{"type":"boolean"},
                "expectedUrl": nullable(string(1, MAX_URL_BYTES)),
                "modifiers": {
                    "type": "array",
                    "maxItems": 4,
                    "uniqueItems": true,
                    "items": {
                        "type": "string",
                        "enum": ["shift", "ctrl", "alt", "meta"]
                    }
                }
            }),
            vec!["sessionId", "pageId"],
        ),
        "click_and_wait_for_popup" => (
            json!({
                "workflowId": id(),
                "commandId": id_pin(),
                "attemptId": id_pin(),
                "sessionId": id(),
                "pageId": id(),
                "selector": string(1, MAX_STRING_BYTES),
                "target": nullable(json!({"$ref":"#/$defs/TargetSpec"})),
                "timeoutMs": timeout_ms(),
                "autoCheckpoint":{"type":"boolean"}
            }),
            vec!["sessionId", "pageId"],
        ),
        "type_text" => (
            json!({
                "workflowId": id(),
                "sessionId": id(),
                "pageId": id(),
                "selector": string(1, MAX_STRING_BYTES),
                "target": nullable(json!({"$ref":"#/$defs/TargetSpec"})),
                "value": string(0, MAX_STRING_BYTES),
                "clearFirst": {"type":"boolean"},
                "expectedUrl": nullable(string(1, MAX_URL_BYTES))
            }),
            vec!["sessionId", "pageId", "value"],
        ),
        "inspect" => (
            json!({
                "workflowId": id(),
                "sessionId": id(),
                "pageId": id(),
                "selector": nullable(string(1, MAX_STRING_BYTES)),
                "target": nullable(json!({"$ref":"#/$defs/TargetSpec"})),
                "includeHtml": {"type":"boolean"}
            }),
            vec!["sessionId", "pageId"],
        ),
        "screenshot" => (
            json!({
                "workflowId": id(),
                "sessionId": id(),
                "pageId": id(),
                "mode": {"$ref":"#/$defs/ScreenshotMode"}
            }),
            vec!["sessionId", "pageId"],
        ),
        "wait_for" => (
            json!({
                "workflowId": id(),
                "sessionId": id(),
                "pageId": id(),
                "condition": {"$ref":"#/$defs/WaitCondition"},
                "timeoutMs": timeout_ms()
            }),
            vec!["sessionId", "pageId", "condition", "timeoutMs"],
        ),
        "page_list" => (
            json!({"sessionId": id(), "workflowId": id()}),
            vec!["sessionId"],
        ),
        "download_url" => (
            json!({
                "workflowId": id(),
                "sessionId": id(),
                "pageId": id(),
                "url": string(1, MAX_URL_BYTES),
                "expectedContentType": nullable(string(1, 256)),
                "maxBytes": {"type":"integer","minimum":1,"maximum":1099511627776_u64},
                "saveAs": nullable(string(1, 4096))
            }),
            vec!["sessionId", "pageId", "url", "maxBytes"],
        ),
        "upload_files" => (
            json!({
                "workflowId": id(),
                "sessionId": id(),
                "pageId": id(),
                "selector": string(1, MAX_STRING_BYTES),
                "target": nullable(json!({"$ref":"#/$defs/TargetSpec"})),
                "controlId": string(1, 128),
                "paths": array(string(1, 4096), 16)
            }),
            vec!["sessionId", "pageId", "paths"],
        ),
        "evaluate_javascript" => (
            json!({
                "workflowId": id(),
                "sessionId": id(),
                "pageId": id(),
                "expression": string(1, MAX_HTML_BYTES),
                "timeoutMs": timeout_ms(),
                "awaitPromise": {"type":"boolean"}
            }),
            vec!["sessionId", "pageId", "expression"],
        ),
        // Intent surface. `command_execute` still accepts the nested
        // `{ kind: "intent", input: … }` form; these build the envelope for you.
        "intent_locate" => (intent_properties(json!({})), intent_required(&["purpose"])),
        "intent_fill" => (
            intent_properties(json!({"value":{"$ref":"#/$defs/FillValue"}})),
            intent_required(&["purpose", "value"]),
        ),
        "intent_complete_form" => (
            intent_properties(json!({
                "fields":nonempty_array(json!({"$ref":"#/$defs/CompleteFormField"}), MAX_COLLECTION_ITEMS),
                "evidenceDetail":{"type":"string","enum":["compact","full"]}
            })),
            intent_required(&["purpose", "fields"]),
        ),
        "intent_submit_and_verify" => (
            intent_properties(json!({
                "expectedState":{"$ref":"#/$defs/WaitForCommand"},
                "autoCheckpoint":{"type":"boolean"}
            })),
            intent_required(&["purpose", "expectedState"]),
        ),
        // The only intent with no purpose/hints of its own.
        "intent_wait_for_state" => (
            intent_scope(json!({
                "condition":{"$ref":"#/$defs/WaitCondition"},
                "timeoutMs":timeout_ms()
            })),
            intent_required(&["condition", "timeoutMs"]),
        ),
        "intent_follow" => (
            intent_properties(json!({
                "expectedDestination":{"$ref":"#/$defs/WaitForCommand"},
                "boundary":{"type":"boolean"},
                "autoCheckpoint":{"type":"boolean"}
            })),
            intent_required(&["purpose", "expectedDestination"]),
        ),
        "intent_dismiss_obstruction" => (
            intent_properties(json!({"timeoutMs":timeout_ms()})),
            intent_required(&["purpose"]),
        ),
        "intent_extract" => (
            intent_properties(json!({
                "fields":nonempty_array(json!({"$ref":"#/$defs/ExtractField"}), MAX_COLLECTION_ITEMS)
            })),
            intent_required(&["purpose", "fields"]),
        ),
        "command_execute" => (
            json!({
                "envelope":{"$ref":"#/$defs/CommandEnvelope"},
                "idempotencyKey":string(1, 128)
            }),
            vec!["envelope"],
        ),
        "checkpoint_save" => (
            json!({
                "checkpoint":{"$ref":"#/$defs/WorkflowCheckpoint"},
                "evidenceRefs":array(id(), MAX_EVIDENCE_ITEMS)
            }),
            vec!["checkpoint"],
        ),
        "workflow_recover" => (json!({"workflowId":id()}), vec!["workflowId"]),
        "job_submit" => (
            json!({
                "name": string(1, MAX_STRING_BYTES),
                "payload": {},
                "priority": {"type":"string","enum":["low","normal","high","critical"]},
                "maxRetries": {"type":"integer","minimum":0,"maximum":32},
                "timeoutMs": timeout_ms()
            }),
            vec!["name"],
        ),
        "job_status" => (json!({"jobId": string(1, 128)}), vec!["jobId"]),
        "job_cancel" => (json!({"jobId": string(1, 128)}), vec!["jobId"]),
        // Either key: `workflowId` for a known workflow, or `sessionId` to
        // discover the recoverable workflows of a session. Neither is required
        // here because exactly-one-of is not expressible in the subset this
        // validator implements; the handler enforces it and names the failure.
        "recovery_status" => (
            json!({
                "workflowId":id(),
                "sessionId":id(),
                "limit":{"type":"integer","minimum":1,"maximum":MAX_RECOVERABLE_WORKFLOWS}
            }),
            vec![],
        ),
        "events_read" => (
            json!({
                "cursor":{"type":"integer","minimum":0},
                "limit":{"type":"integer","minimum":1,"maximum":MAX_EVENT_LIMIT}
            }),
            vec!["limit"],
        ),
        _ => (json!({}), vec![]),
    };
    let mut schema = object(properties, &required);
    schema["$schema"] = json!("https://json-schema.org/draft/2020-12/schema");
    schema["$defs"] = reachable_definitions(&schema);
    schema
}

/// The schema advertised in `tools/list`, distinct from the schema
/// `validate_tool_arguments` enforces at dispatch.
///
/// Narrows deep `$defs` that dominate connect cost (`WaitCondition`,
/// `IntentHints`, `TargetSpec`, `ScreenshotMode`) and, for `command_execute`,
/// collapses `envelope.command` to an opaque object instead of the full
/// `PrimitiveCommand`/`IntentCommand` union. Top-level property names and
/// required fields stay identical. `tool_schema` / `definitions()` keep the
/// full shapes, so `tools/call` validation is unchanged. The four primitives
/// with no named tool are documented in `bobby://primitives`.
pub(crate) fn advertised_tool_schema(name: &str) -> Value {
    let mut schema = tool_schema(name);
    // The draft URL is constant and MCP does not require it per tool; the
    // validation schema keeps it. ~80 bytes x 47 tools of pure connect fat.
    schema
        .as_object_mut()
        .expect("tool schema is an object")
        .remove("$schema");
    let mut patched = definitions();
    let patched = patched.as_object_mut().expect("definitions is an object");
    apply_advertised_input_patches(patched);
    if workflow_scope_for_tool(name).is_some() {
        patched.insert("H".to_owned(), workflow_handle());
    }
    if name == "command_execute" {
        if let Some(command_envelope) = patched.get_mut("CommandEnvelope") {
            command_envelope["properties"]["command"] = json!({
                "type": "object",
                "description": "One command as {\"kind\":\"primitive\"|\"intent\",\"input\":{…}}. \
            Prefer named tools for common actions; they build this envelope."
            });
        }
    }
    if name == "workflow_observe" {
        schema["properties"]["goal"]["maxLength"] = json!(MAX_WORKFLOW_GOAL_SCALARS);
    }
    if name == "workflow_start" {
        schema["properties"]["proxy"] = json!({"type":["string","null"],"maxLength":2048});
        schema["properties"]["executionPolicy"]
            .as_object_mut()
            .expect("execution policy is an object schema")
            .remove("required");
    }
    apply_workflow_scope_advertisement(name, &mut schema);
    let seed = json!({"properties": schema["properties"], "required": schema["required"]});
    let defs = reachable_definitions_from(&seed, patched);
    if defs.as_object().is_some_and(|defs| !defs.is_empty()) {
        schema["$defs"] = defs;
    } else {
        schema
            .as_object_mut()
            .expect("tool schemas are objects")
            .remove("$defs");
    }
    if let Some(example) = tool_argument_example(name) {
        schema["examples"] = json!([example]);
    }
    schema
}

/// Capability-sensitive advertising for the two session-creation contracts.
/// Validation stays independent of what an individual principal can see.
pub(crate) fn advertised_tool_schema_for_capabilities(
    name: &str,
    capabilities: &types::CapabilitySet,
) -> Value {
    let mut schema = advertised_tool_schema(name);
    if matches!(name, "session_create" | "workflow_start") {
        let policy = &mut schema["properties"]["executionPolicy"]["properties"];
        if let Some(policy) = policy.as_object_mut() {
            if !capabilities.contains(types::Capability::BrowserFingerprint) {
                policy.remove("fingerprint");
            }
            if !capabilities.contains(types::Capability::BrowserHumanize) {
                policy.remove("humanize");
            }
        }
        // Godmode forces fingerprint + humanize server-side, so it requires
        // both capabilities at creation. A principal missing either must not
        // see the flag it cannot use.
        if !capabilities.contains(types::Capability::BrowserFingerprint)
            || !capabilities.contains(types::Capability::BrowserHumanize)
        {
            if let Some(properties) = schema["properties"].as_object_mut() {
                properties.remove("zigzagzig");
            }
        }
    }
    schema
}

pub(crate) fn apply_runtime_tool_limits(name: &str, schema: &mut Value, max_download_bytes: usize) {
    if name != "download_url" {
        return;
    }
    let maximum = json!(max_download_bytes);
    schema["properties"]["maxBytes"]["maximum"] = maximum.clone();
    if let Some(branches) = schema.get_mut("oneOf").and_then(Value::as_array_mut) {
        for branch in branches {
            branch["properties"]["maxBytes"]["maximum"] = maximum.clone();
        }
    }
}

/// Adds the handle alternative only to the advertised schema. The dispatcher
/// continues to validate normalized explicit ids against `tool_schema`.
fn apply_workflow_scope_advertisement(name: &str, schema: &mut Value) {
    let Some(scope) = workflow_scope_for_tool(name) else {
        return;
    };
    let business_properties = schema["properties"]
        .as_object()
        .expect("tool schemas have object properties")
        .clone();
    let properties = schema["properties"]
        .as_object_mut()
        .expect("tool schemas have object properties");
    properties.insert("workflowHandle".to_owned(), json!({"$ref":"#/$defs/H"}));

    let required = schema["required"]
        .as_array_mut()
        .expect("tool schemas have required arrays");
    required.retain(|field| field != "sessionId" && field != "pageId");

    let explicit_required = match scope {
        WorkflowScope::SessionPage => json!(["sessionId", "pageId"]),
        WorkflowScope::SessionPageWorkflow => json!(["sessionId", "pageId", "workflowId"]),
    };
    let mut handle_properties = business_properties.clone();
    handle_properties.insert("workflowHandle".to_owned(), json!({"$ref":"#/$defs/H"}));
    handle_properties.insert("sessionId".to_owned(), Value::Bool(false));
    handle_properties.insert("pageId".to_owned(), Value::Bool(false));
    handle_properties.insert("workflowId".to_owned(), Value::Bool(false));

    let mut explicit_properties = business_properties;
    explicit_properties.insert("workflowHandle".to_owned(), Value::Bool(false));
    schema["oneOf"] = json!([
        {
            "required":["workflowHandle"],
            "properties":handle_properties
        },
        {
            "required":explicit_required,
            "properties":explicit_properties
        }
    ]);
}

/// Advertised workflow-handle shape. Runtime lookup intentionally uses the
/// allocation-free parser in `workflow_handles.rs`, not a JSON Schema pattern.
fn workflow_handle() -> Value {
    json!({
        "type":"string",
        "minLength":35,
        "maxLength":35,
        "description":"wf_ + 32 lowercase hex."
    })
}

/// Opaque / property-preserving stand-ins for the largest input `$defs`. Used
/// only by [`advertised_tool_schema`]; [`definitions`] stays full for validation.
fn apply_advertised_input_patches(patched: &mut Map<String, Value>) {
    patched.insert(
        "IntentHints".to_owned(),
        object(
            json!({
                "role":nullable(string(0, 256)),
                "accessibleName":nullable(string(0, 256)),
                "nearText":nullable(json!({"type":"object"})),
                "ordinal":nullable(json!({"type":"integer","minimum":0,"maximum":1000000})),
                "framePath":{"type":"array","items":{"type":"object"},"maxItems":16},
                "shadowPath":{"type":"array","items":{"type":"object"},"maxItems":16},
                "allowBestMatch":{"type":"boolean"}
            }),
            &[],
        ),
    );
    patched.insert(
        "WaitCondition".to_owned(),
        // Not opaque: an agent must be able to author a condition without
        // guessing `kind` tags. Tags, required fields, and enums are real;
        // nested shapes (`target`, `matcher`) stay generic — the full union
        // is enforced at tools/call. Closed with the full property list per
        // variant, per the repo's advertised-schema invariant.
        json!({"oneOf":[
            {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"element"},"target":{"type":"object"},"state":{"type":"string","enum":["attached","detached","visible","hidden","enabled","disabled"]}},"required":["kind","target","state"]},
            {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"text"},"target":{"type":"object"},"matcher":{"$ref":"#/$defs/TextMatch"}},"required":["kind","target","matcher"]},
            {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"value"},"target":{"type":"object"},"matcher":{"$ref":"#/$defs/TextMatch"}},"required":["kind","target","matcher"]},
            {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"url"},"matcher":{"$ref":"#/$defs/TextMatch"}},"required":["kind","matcher"]},
            {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"document"},"ready":{"type":"string","enum":["commit","domContentLoaded","interactive","networkIdle"]}},"required":["kind","ready"]},
            {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"networkQuiet"},"idleMs":{"type":"integer"},"maxInFlight":{"type":"integer"},"ignoreUrlSubstrings":{"type":"array"},"ignoreResourceTypes":{"type":"array"},"ignoreLongLived":{"type":"boolean"}},"required":["kind","idleMs","maxInFlight"]}
        ]}),
    );
    patched.insert(
        "TargetSpec".to_owned(),
        json!({
            "type":"object",
            "description":"Resolved target or selector path. Full TargetSpec enforced at tools/call."
        }),
    );
    patched.insert(
        "ScreenshotMode".to_owned(),
        json!({
            "type":"object",
            "description":"viewport | fullPage | element | clip. Full union enforced at tools/call."
        }),
    );
}

/// Compact argument templates for the first-run tools. Kept off the validation
/// schema path so `validate_tool_arguments` stays lean; only `tools/list`
/// advertises them. Limit these to the setup sequence so the full surface keeps
/// enough room for another small tool.
fn tool_argument_example(name: &str) -> Option<Value> {
    let session = "10000000-0000-4000-8000-000000000001";
    Some(match name {
        "session_create" => json!({
            "profile": "default"
        }),
        "page_open" => json!({
            "sessionId": session,
            "url": "https://example.test/"
        }),
        "workflow_start" => json!({"profile":"default"}),
        "workflow_observe" => json!({
            "workflowHandle":"wf_0123456789abcdef0123456789abcdef"
        }),
        _ => return None,
    })
}

/// The shape of a tool's `structuredContent`, resolved through the same
/// `reachable_definitions` closure as the input schemas: a blanket `$defs` here would
/// undo the payload reduction the input side pays for. Every tool returns structured
/// content, hence `Value` and not `Option<Value>`.
pub(crate) fn tool_output_schema(name: &str) -> Value {
    let mut schema = match name {
        "runtime_info" => object(
            json!({
                "version":string(0, 256),
                "capabilities":array(string(0, 64), 32),
                "active_sessions":{"type":"integer","minimum":0},
                "queued_jobs":{"type":"integer","minimum":0},
                "uptime_ms":{"type":"integer","minimum":0},
                "credentialExpiresAt":{"type":"string","format":"date-time"}
            }),
            &[
                "version",
                "capabilities",
                "active_sessions",
                "queued_jobs",
                "uptime_ms",
                "credentialExpiresAt",
            ],
        ),
        // MCP types `structuredContent` as an object, so the array is wrapped
        // under `sessions`. `GET /v1/sessions` still returns a bare array.
        "session_list" => object(
            json!({"sessions":{"type":"array","items":{"$ref":"#/$defs/SessionState"}}}),
            &["sessions"],
        ),
        "session_create" => output_ref("SessionState"),
        "workflow_start" => workflow_start_output_schema(),
        "workflow_observe" => object(
            json!({
                "status":string(1, 32),
                "source":string(1, 128),
                "workflowHandle":workflow_handle(),
                "sessionId":id(),
                "pageId":id(),
                "workflowId":id(),
                "retainedAnswer":nullable(json!({"type":"object"})),
                "observationOutcome":workflow_observation_outcome_schema(),
                "formSnapshot":nullable(json!({"$ref":"#/$defs/FormSnapshot"}))
            }),
            &[
                "status",
                "source",
                "workflowHandle",
                "sessionId",
                "pageId",
                "workflowId",
                "retainedAnswer",
                "observationOutcome",
                "formSnapshot",
            ],
        ),
        "session_close" => object(
            json!({"closed":{"type":"boolean","const":true}}),
            &["closed"],
        ),
        // `PageState`'s own fields plus `navigationOutcome` (present whenever `url`
        // was given) and, only if that navigation did not complete, `cleanupOutcome`
        // and `pageClosed`. Those three are declared but not required.
        //
        // Must be inlined into `page_state()`'s `properties`, not `allOf`-ed as a
        // `$ref`: JSON Schema 2020-12 `additionalProperties` is not annotation-aware
        // across `allOf`, so `PageState`'s `additionalProperties:false` would reject
        // `navigationOutcome` on every navigated open.
        "page_open" => {
            let mut schema = page_state();
            merge_properties(
                &mut schema["properties"],
                json!({
                    "navigationOutcome":object(command_outcome_properties(), &["status", "commandId"]),
                    "cleanupOutcome":object(command_outcome_properties(), &["status", "commandId"]),
                    "pageClosed":{"type":"boolean"}
                }),
            );
            schema
        }
        "checkpoint_save" => output_ref("CheckpointRecord"),
        "workflow_recover" => output_ref("RecoveryDecision"),
        "job_submit" => object(
            json!({
                "jobId": string(1, 128),
                "status": string(1, 32)
            }),
            &["jobId", "status"],
        ),
        "job_status" => object(
            json!({
                "id": string(1, 128),
                "name": string(1, MAX_STRING_BYTES),
                "status": string(1, 32)
            }),
            &["id", "name", "status"],
        ),
        "job_cancel" => object(
            json!({
                "cancelled": {"type":"boolean"},
                "jobId": string(1, 128)
            }),
            &["cancelled", "jobId"],
        ),
        "recovery_status" => object(
            json!({
                "workflowId":id(),
                "checkpoint":{"$ref":"#/$defs/CheckpointRecord"},
                // `RecoveryReceipt` also carries a `CommandOutcome`, a
                // `SkillOutcome`, and a `SkillDecision`; kept generic here
                // for the same reason as `CheckpointRecord.recoveryReceipts`.
                "receipts":array(json!({"type":"object"}), MAX_RECOVERY_RECEIPTS),
                // Present instead of the three above when the caller asked by
                // `sessionId`. Nothing is required because the two branches
                // share no field.
                "workflows":array(id(), MAX_RECOVERABLE_WORKFLOWS)
            }),
            &[],
        ),
        "events_read" => object(
            json!({
                "events":array(
                    object(
                        json!({
                            "cursor":{"type":"integer","minimum":0},
                            "kind":string(1, 128),
                            "payload":any_value()
                        }),
                        &["cursor", "kind", "payload"],
                    ),
                    MAX_EVENT_LIMIT,
                ),
                "latestAvailable":{"type":"integer","minimum":0}
            }),
            &["events", "latestAvailable"],
        ),
        // Not `page_list` / `page_close` / `page_activate`: unlike
        // `page_open`, all three dispatch through `submit_envelope` just
        // like every primitive/intent command below, so their real output
        // is `CommandOutcome` plus `workflowId`, not a `PageState`.
        "form_snapshot" => output_ref("FormSnapshot"),
        // Every other tool (~30) submits a command envelope through `submit_envelope`,
        // which returns the envelope's `CommandOutcome` with `workflowId` inserted at
        // the top level. Stays self-contained (no `$ref`, no `$defs`) for the reason
        // given on `command_outcome_properties`.
        _ => object(
            {
                let mut properties = command_outcome_properties();
                merge_properties(&mut properties, json!({"workflowId": id()}));
                properties
            },
            &["status", "commandId", "workflowId"],
        ),
    };
    schema["$schema"] = json!("https://json-schema.org/draft/2020-12/schema");
    // `definitions()` keeps the full `Evidence` union for validation, but reaching it
    // from an output schema blows the connect budget. Patch a copy of the table down to
    // the tag-only projection before resolving reachability, leaving `definitions()`
    // itself untouched.
    let mut patched = definitions();
    let patched = patched.as_object_mut().expect("definitions is an object");
    patched.insert(
        "Evidence".to_owned(),
        json!({"oneOf": evidence_variant_tags()}),
    );
    let defs = reachable_definitions_from(&schema, patched);
    if defs.as_object().is_some_and(|defs| !defs.is_empty()) {
        schema["$defs"] = defs;
    }
    schema
}

/// `tools/list` outputSchema. Identical to [`tool_output_schema`] except
/// advertise-only collapses for tools whose nested `$defs` dominate the entry
/// (`form_snapshot`, `workflow_recover`, `recovery_status`, `page_open`,
/// `session_create`, `session_list`, `checkpoint_save`); wire types stay
/// unchanged. Every entry also drops the constant `$schema` URL (see
/// [`advertised_tool_schema`]).
pub(crate) fn advertised_tool_output_schema(name: &str) -> Value {
    match name {
        "workflow_recover" => {
            let mut schema = output_ref("RecoveryDecision");
            let mut patched = definitions();
            let patched = patched.as_object_mut().expect("definitions is an object");
            patched.insert(
                "RecoveryDecision".to_owned(),
                json!({"oneOf": recovery_decision_tags()}),
            );
            let defs = reachable_definitions_from(&schema, patched);
            if defs.as_object().is_some_and(|defs| !defs.is_empty()) {
                schema["$defs"] = defs;
            }
            schema
        }
        "form_snapshot" => {
            let mut schema = output_ref("FormSnapshot");
            let mut patched = definitions();
            let patched = patched.as_object_mut().expect("definitions is an object");
            patched.insert("FormSnapshot".to_owned(), advertised_form_snapshot());
            let defs = reachable_definitions_from(&schema, patched);
            if defs.as_object().is_some_and(|defs| !defs.is_empty()) {
                schema["$defs"] = defs;
            }
            schema
        }
        "session_create" | "checkpoint_save" => {
            // Opaque object: SessionState / CheckpointRecord pull large nested
            // defs into tools/list; validation still uses the full schema.
            json!({"type":"object"})
        }
        "session_list" => object(
            json!({"sessions":array(json!({"type":"object"}), MAX_COLLECTION_ITEMS)}),
            &["sessions"],
        ),
        "recovery_status" => {
            let mut schema = object(
                json!({
                    "workflowId":id(),
                    "checkpoint":json!({"type":"object"}),
                    "receipts":array(json!({"type":"object"}), MAX_RECOVERY_RECEIPTS),
                    "workflows":array(json!({"type":"object"}), MAX_RECOVERABLE_WORKFLOWS)
                }),
                &[],
            );
            // Carries the definition `id()` points at, as `page_open` does.
            let defs = reachable_definitions(&schema);
            if defs.as_object().is_some_and(|defs| !defs.is_empty()) {
                schema["$defs"] = defs;
            }
            schema
        }
        "page_open" => {
            let mut schema = object(
                json!({
                    "id":id(),
                    "session_id":id(),
                    "url":nullable(string(0, MAX_URL_BYTES)),
                    "mode":{"type":"string","enum":["Document","Interactive","Render"]},
                    "ready_state":string(0, 256),
                    "pending_requests":{"type":"integer","minimum":0},
                    "navigationOutcome":json!({"type":"object"}),
                    "cleanupOutcome":json!({"type":"object"}),
                    "pageClosed":{"type":"boolean"}
                }),
                &[
                    "id",
                    "session_id",
                    "url",
                    "mode",
                    "ready_state",
                    "pending_requests",
                ],
            );
            // `id()` is a `$ref`, so this arm has to carry the definition it
            // points at. Returning the object bare left `#/$defs/Id` dangling.
            let defs = reachable_definitions(&schema);
            if defs.as_object().is_some_and(|defs| !defs.is_empty()) {
                schema["$defs"] = defs;
            }
            schema
        }
        // Workflow handles (this branch): the start schema is authored
        // directly, and observe carries the same FormSnapshot collapse
        // form_snapshot uses.
        "workflow_start" => advertised_workflow_start_output_schema(),
        "workflow_observe" => {
            let mut schema = tool_output_schema(name);
            let mut seed = schema.clone();
            seed.as_object_mut()
                .expect("output schemas are objects")
                .remove("$defs");
            let mut patched = definitions();
            let patched = patched.as_object_mut().expect("definitions is an object");
            patched.insert("FormSnapshot".to_owned(), advertised_form_snapshot());
            let defs = reachable_definitions_from(&seed, patched);
            if defs.as_object().is_some_and(|defs| !defs.is_empty()) {
                schema["$defs"] = defs;
            }
            schema
        }
        _ => {
            let mut schema = tool_output_schema(name);
            schema
                .as_object_mut()
                .expect("output schema is an object")
                .remove("$schema");
            schema
        }
    }
}

/// Exact advertise-only workflow-start result. The three closed wire branches
/// are unchanged from [`workflow_start_output_schema`], but repeated nested
/// session/page/navigation schemas and failure fields are shared through local
/// definitions so the public catalog does not duplicate them per branch.
fn advertised_workflow_start_output_schema() -> Value {
    let all_defs = definitions();
    let id_def = all_defs["Id"].clone();
    let mut session_def = all_defs["SessionState"].clone();
    let mut navigation_def = object(command_outcome_properties(), &["status", "commandId"]);
    navigation_def["type"] = json!(["object", "null"]);
    let mut page_def = page_state();
    rewrite_local_id_refs(&mut session_def);
    rewrite_local_id_refs(&mut navigation_def);
    rewrite_local_id_refs(&mut page_def);
    let mut handle = workflow_handle();
    handle
        .as_object_mut()
        .expect("workflow handle schema is an object")
        .remove("description");

    let success = object(
        json!({
            "status":{"const":"completed"},
            "workflowHandle":handle,
            "sessionId":{"$ref":"#/$defs/I"},
            "pageId":{"$ref":"#/$defs/I"},
            "workflowId":{"$ref":"#/$defs/I"},
            "session":{"$ref":"#/$defs/S"},
            "page":{"$ref":"#/$defs/P"},
            "navigationOutcome":{"$ref":"#/$defs/N"}
        }),
        &[
            "status",
            "workflowHandle",
            "sessionId",
            "pageId",
            "workflowId",
            "session",
            "page",
            "navigationOutcome",
        ],
    );
    // Deliberately open only as a shared applicator. Each concrete failure
    // branch below closes the union with `unevaluatedProperties:false` after
    // adding its page/reason fields.
    let failure_base = json!({
        "type":"object",
        "properties":{
            "status":{"const":"failed"},
            "workflowHandle":{"type":"null"},
            "sessionId":{"$ref":"#/$defs/I"},
            "workflowId":{"$ref":"#/$defs/I"},
            "session":{"oneOf":[{"$ref":"#/$defs/S"},{"type":"null"}]},
            "navigationOutcome":{"$ref":"#/$defs/N"},
            "reason":{"type":"string"},
            "detail":{"type":["string","null"],"minLength":1,"maxLength":512},
            "pageClosed":{"type":"boolean"},
            "sessionDeleted":{"type":"boolean"},
            "cleanupErrorCode":{"type":["string","null"],"minLength":1,"maxLength":128}
        },
        "required":["status","workflowHandle","sessionId","workflowId","session","navigationOutcome","reason","detail","pageClosed","sessionDeleted","cleanupErrorCode"]
    });
    let page_open_failed = json!({
        "$ref":"#/$defs/F",
        "properties":{
            "pageId":{"type":"null"},
            "page":{"type":"null"},
            "reason":{"const":"pageOpenFailed"}
        },
        "required":["pageId","page","reason"],
        "unevaluatedProperties":false
    });
    let later_failure = json!({
        "$ref":"#/$defs/F",
        "properties":{
            "pageId":{"$ref":"#/$defs/I"},
            "page":{"$ref":"#/$defs/P"},
            "reason":{"enum":["navigationFailed","workflowGenerationChanged","workflowSupervisorLost"]}
        },
        "required":["pageId","page","reason"],
        "unevaluatedProperties":false
    });
    json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "type":"object",
        "oneOf":[
            {"$ref":"#/$defs/C"},
            {"$ref":"#/$defs/O"},
            {"$ref":"#/$defs/L"}
        ],
        "$defs":{
            "I":id_def,
            "S":session_def,
            "N":navigation_def,
            "P":page_def,
            "F":failure_base,
            "C":success,
            "O":page_open_failed,
            "L":later_failure
        }
    })
}

fn rewrite_local_id_refs(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            if fields.get("$ref").and_then(Value::as_str) == Some("#/$defs/Id") {
                fields.insert("$ref".into(), json!("#/$defs/I"));
            }
            for value in fields.values_mut() {
                rewrite_local_id_refs(value);
            }
        }
        Value::Array(values) => values.iter_mut().for_each(rewrite_local_id_refs),
        _ => {}
    }
}

fn workflow_start_output_schema() -> Value {
    let navigation_outcome = nullable(object(
        command_outcome_properties(),
        &["status", "commandId"],
    ));
    let stable_failure = json!({
        "workflowHandle":{"type":"null"},
        "sessionId":id(),
        "workflowId":id(),
        "session":nullable(json!({"$ref":"#/$defs/SessionState"})),
        "navigationOutcome":navigation_outcome,
        "reason":{"type":"string","enum":["pageOpenFailed","navigationFailed","workflowGenerationChanged","workflowSupervisorLost"]},
        "detail":nullable(string(1, 512)),
        "pageClosed":{"type":"boolean"},
        "sessionDeleted":{"type":"boolean"},
        "cleanupErrorCode":nullable(string(1, 128))
    });
    let success = status_fields(
        "completed",
        json!({
            "workflowHandle":workflow_handle(),
            "sessionId":id(),
            "pageId":id(),
            "workflowId":id(),
            "session":{"$ref":"#/$defs/SessionState"},
            "page":page_state(),
            "navigationOutcome":navigation_outcome
        }),
        &[
            "workflowHandle",
            "sessionId",
            "pageId",
            "workflowId",
            "session",
            "page",
            "navigationOutcome",
        ],
    );
    let page_open_failed = status_fields(
        "failed",
        merge_values(
            stable_failure.clone(),
            json!({
                "pageId":{"type":"null"},
                "page":{"type":"null"},
                "reason":{"const":"pageOpenFailed"}
            }),
        ),
        &[
            "workflowHandle",
            "sessionId",
            "workflowId",
            "session",
            "pageId",
            "page",
            "navigationOutcome",
            "reason",
            "pageClosed",
            "sessionDeleted",
            "cleanupErrorCode",
        ],
    );
    let later_failure = status_fields(
        "failed",
        merge_values(
            stable_failure,
            json!({
                "pageId":id(),
                "page":page_state(),
                "reason":{"type":"string","enum":["navigationFailed","workflowGenerationChanged","workflowSupervisorLost"]}
            }),
        ),
        &[
            "workflowHandle",
            "sessionId",
            "workflowId",
            "session",
            "pageId",
            "page",
            "navigationOutcome",
            "reason",
            "pageClosed",
            "sessionDeleted",
            "cleanupErrorCode",
        ],
    );
    json!({"type":"object","oneOf":[success,page_open_failed,later_failure]})
}

fn workflow_observation_outcome_schema() -> Value {
    let mut properties = command_outcome_properties();
    merge_properties(&mut properties, json!({"workflowId":id()}));
    nullable(object(properties, &["status", "commandId", "workflowId"]))
}

fn merge_values(mut left: Value, right: Value) -> Value {
    left.as_object_mut()
        .expect("schema fragments are objects")
        .extend(
            right
                .as_object()
                .expect("schema fragments are objects")
                .clone(),
        );
    left
}

/// Advertise-only FormSnapshot: keep top-level keys, collapse nested controls.
fn advertised_form_snapshot() -> Value {
    object(
        json!({
            "schemaVersion":{"type":"integer","const":1},
            "pageId":id(),
            "forms":array(json!({"type":"object"}), 64),
            "unownedControls":array(json!({"type":"object"}), 512),
            "truncated":{"type":"boolean"}
        }),
        &[
            "schemaVersion",
            "pageId",
            "forms",
            "unownedControls",
            "truncated",
        ],
    )
}

/// A top-level schema whose entire value equals one named definition. JSON Schema
/// 2020-12 treats `$ref` as an ordinary applicator, so a sibling `type` still applies,
/// satisfying "every outputSchema has `type: object`" without a wrapper key.
fn output_ref(def_name: &str) -> Value {
    json!({"type":"object","$ref":format!("#/$defs/{def_name}")})
}

/// Every intent tool is page-scoped, so the scope keys are always required.
fn intent_required(extra: &[&'static str]) -> Vec<&'static str> {
    let mut required = vec!["sessionId", "pageId"];
    required.extend_from_slice(extra);
    required
}

/// Page scope shared by every intent tool.
///
/// `workflowId` is optional on the way in so an agent can keep a multi-step
/// intent sequence inside one workflow and then checkpoint it. `commandId`
/// and `attemptId` are optional too: a Boundary intent's pre-action
/// checkpoint must name the exact ids the submit will carry, so the caller
/// pins them up front instead of letting the server mint them.
fn intent_scope(extra: Value) -> Value {
    let mut properties = json!({
        "sessionId": id(),
        "pageId": id(),
        "workflowId": id(),
        "commandId": id_pin(),
        "attemptId": id_pin(),
        "idempotencyKey": string(1, 128)
    });
    merge_properties(&mut properties, extra);
    properties
}

fn intent_properties(extra: Value) -> Value {
    let mut properties = intent_scope(json!({
        "purpose": string(1, 256),
        "hints": {"$ref":"#/$defs/IntentHints"}
    }));
    merge_properties(&mut properties, extra);
    properties
}

fn merge_properties(properties: &mut Value, extra: Value) {
    let Some(target) = properties.as_object_mut() else {
        return;
    };
    let Value::Object(extra) = extra else {
        return;
    };
    target.extend(extra);
}

/// Emits only the definitions a tool can actually reach; a full `$defs` block on every
/// schema pushes `tools/list` past the frame cap.
///
/// The closure must stay transitive: `validate` resolves `$ref` against `root.$defs` and
/// fails closed on a missing target, so a partial closure rejects valid arguments.
fn reachable_definitions(schema: &Value) -> Value {
    let all = definitions();
    let all = all.as_object().expect("definitions is an object");
    reachable_definitions_from(schema, all)
}

/// Same closure walk as [`reachable_definitions`], sourced from an explicit definitions
/// map so a caller can narrow what it advertises without disturbing [`definitions()`],
/// which [`tool_schema`] needs unmodified for edge validation.
fn reachable_definitions_from(schema: &Value, all: &Map<String, Value>) -> Value {
    let mut pending = BTreeSet::new();
    collect_refs(schema, &mut pending);
    let mut reachable = Map::new();
    while let Some(name) = pending.pop_first() {
        if reachable.contains_key(&name) {
            continue;
        }
        let Some(definition) = all.get(&name) else {
            continue;
        };
        collect_refs(definition, &mut pending);
        reachable.insert(name, definition.clone());
    }
    Value::Object(reachable)
}

fn collect_refs(value: &Value, found: &mut BTreeSet<String>) {
    match value {
        Value::Object(fields) => {
            if let Some(name) = fields
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|reference| reference.strip_prefix("#/$defs/"))
            {
                found.insert(name.to_owned());
            }
            for field in fields.values() {
                collect_refs(field, found);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_refs(item, found);
            }
        }
        _ => {}
    }
}

/// Test-only accessor for the full `definitions()` table.
pub(crate) fn definitions_for_test() -> Value {
    definitions()
}

fn definitions() -> Value {
    json!({
        "Id": {"type":"string","format":"uuid","minLength":36,"maxLength":36},
        "CommandEnvelope": object(json!({
            "schemaVersion":{"type":"integer","const":2},
            "commandId":id(), "workflowId":id(), "attemptId":id(), "sessionId":id(),
            "pageId":nullable(id()),
            "deadline":{"type":"string","format":"date-time","minLength":20,"maxLength":64},
            "command":{"$ref":"#/$defs/RuntimeCommand"}
        }), &["schemaVersion","commandId","workflowId","attemptId","sessionId","deadline","command"]),
        "RuntimeCommand": {"oneOf": runtime_commands()},
        "PrimitiveCommand": {"oneOf": primitive_commands()},
        "IntentCommand": {"oneOf": intent_commands()},
        "IntentHints": intent_hints(),
        "FillValue": {"oneOf": fill_values()},
        "CompleteFormField": complete_form_field(),
        "ExtractField": extract_field(),
        "ExtractValueKind": {"oneOf": extract_value_kinds()},
        "AccessibilityTarget": accessibility_target(),
        "ViewportSize": object(json!({
            "width":{"type":"integer","minimum":1,"maximum":16384},
            "height":{"type":"integer","minimum":1,"maximum":16384}
        }), &["width", "height"]),
        "GeolocationCoordinates": object(json!({
            "latitude":{"type":"number","minimum":-90,"maximum":90},
            "longitude":{"type":"number","minimum":-180,"maximum":180},
            "accuracy":nullable(json!({"type":"number","minimum":0}))
        }), &["latitude", "longitude"]),
        "CookieRecord": object(json!({
            "name":string(0, 1024),
            "value":string(0, 4096),
            "domain":string(0, 1024),
            "path":string(0, 4096),
            "secure":{"type":"boolean"},
            "httpOnly":{"type":"boolean"},
            "sameSite":nullable(string(0, 32)),
            "expiresUnix":nullable(json!({"type":"number"}))
        }), &["name", "value", "domain", "path", "secure", "httpOnly"]),
        "SetCookieParam": object(json!({
            "name":string(1, 1024),
            "value":string(0, 4096),
            "url":string(1, MAX_URL_BYTES),
            "path":nullable(string(0, 4096)),
            "secure":{"type":"boolean"},
            "httpOnly":{"type":"boolean"},
            "sameSite":nullable(string(0, 32)),
            "expiresUnix":nullable(json!({"type":"number"}))
        }), &["name", "value", "url"]),
        "AccessibilityNode": accessibility_node(),
        "WaitForCommand": wait_for_command(),
        "TargetSpec": target_spec(),
        "TextMatch": {"oneOf":[
            tagged_content("exact", string(0, MAX_STRING_BYTES)),
            tagged_content("contains", string(0, MAX_STRING_BYTES)),
            tagged_content("regex", string(0, MAX_STRING_BYTES))
        ]},
        "WaitCondition": {"oneOf": wait_conditions()},
        "ScreenshotMode": {"oneOf": screenshot_modes()},
        "ExecutionRecord": execution_record(),
        "CheckpointInvariant": {"oneOf":[
            tagged_fields("url", json!({"value":string(1, MAX_URL_BYTES)}), &["value"]),
            tagged_fields("title", json!({"value":string(0, MAX_STRING_BYTES)}), &["value"]),
            tagged_fields("text", json!({"selector":string(1, MAX_STRING_BYTES),"value":string(0, MAX_STRING_BYTES)}), &["selector","value"])
        ]},
        "RecoveryDecision": {"oneOf": recovery_decisions()},
        "RecoveryRecord": object(json!({
            "recordedAt":{"type":"string","format":"date-time","minLength":20,"maxLength":64},
            "decision":{"$ref":"#/$defs/RecoveryDecision"}
        }), &["recordedAt","decision"]),
        "WorkflowCheckpoint": workflow_checkpoint(),
        // --- Output-only definitions ------------------------------------------
        // Everything below describes `structuredContent`, never a tool argument.
        // `Evidence` is kept here full and field-for-field with `types::Evidence` so
        // validation and the drift guard see the truth, and so
        // `recovery_decisions()`'s `$ref:"#/$defs/Evidence"` resolves. Advertised
        // output schemas resolve the tag-only `evidence_variant_tags` projection
        // instead, so this union never reaches a `tools/list` payload.
        //
        // `CommandOutcome` has no named definition: ~31 tools submit a command
        // envelope and would each carry the `$ref`, so `tool_output_schema`'s fallback
        // arm and `page_open` inline `command_outcome_properties()` instead.
        "Evidence": {"oneOf": evidence_variants()},
        "SessionState": session_state(),
        "PageState": page_state(),
        "CheckpointRecord": checkpoint_record(),
        "FormSnapshot": form_snapshot_schema(),
        "FormDescriptor": form_descriptor(),
        "FormControl": form_control(),
        "FormControlTarget": form_control_target(),
        "FormControlState": {"oneOf": form_control_state_variants()},
        "FormControlValidity": form_control_validity(),
        "FormValidationIssue": form_validation_issue(),
        "FormOption": form_option()
    })
}

fn workflow_checkpoint() -> Value {
    object(
        json!({
                "schemaVersion":{"type":"integer","const":1},
                "checkpointId":id(), "workflowId":id(), "attemptId":id(), "sessionId":id(), "pageId":id(),
                "restartUrl":string(1, MAX_URL_BYTES), "currentUrl":string(1, MAX_URL_BYTES),
                "cursor":nullable(id()), "boundaryCommandId":nullable(id()),
                "recoveryClass":{"type":"string","enum":["replayable","reconciliable","boundary"]},
                "invariants":array(json!({"$ref":"#/$defs/CheckpointInvariant"}), MAX_COLLECTION_ITEMS),
                "replayableInputs":array(string(0, MAX_STRING_BYTES), MAX_COLLECTION_ITEMS),
                // Caller-authored values are never honored: `RecoveryCoordinator::
                // save_verified` overwrites `evidence` from `evidenceRefs`, and the
                // runtime alone appends recovery history. Forcing all three empty also
                // drops `Evidence`/`RecoveryDecision`/`RecoveryRecord` out of this
                // tool's reachable `$defs`.
                "evidence":{"type":"array","maxItems":0},
                "recoveryHistory":{"type":"array","maxItems":0},
                "recoveryReceipts":{"type":"array","maxItems":0},
                "createdAt":{"type":"string","format":"date-time","minLength":20,"maxLength":64}
        }),
        &[
            "schemaVersion",
            "checkpointId",
            "workflowId",
            "attemptId",
            "sessionId",
            "pageId",
            "restartUrl",
            "currentUrl",
            "recoveryClass",
            "invariants",
            "replayableInputs",
            "evidence",
            "createdAt",
        ],
    )
}

fn runtime_commands() -> Vec<Value> {
    vec![
        tagged_input("primitive", json!({"$ref":"#/$defs/PrimitiveCommand"})),
        tagged_input("intent", json!({"$ref":"#/$defs/IntentCommand"})),
    ]
}

fn intent_commands() -> Vec<Value> {
    vec![
        tagged_input(
            "locate",
            object(
                json!({
                    "purpose":string(1, 256),
                    "hints":{"$ref":"#/$defs/IntentHints"}
                }),
                &["purpose"],
            ),
        ),
        tagged_input(
            "fill",
            object(
                json!({
                    "purpose":string(1, 256),
                    "hints":{"$ref":"#/$defs/IntentHints"},
                    "value":{"$ref":"#/$defs/FillValue"}
                }),
                &["purpose", "value"],
            ),
        ),
        tagged_input(
            "completeForm",
            object(
                json!({
                    "purpose":string(1, 256),
                    "fields":nonempty_array(json!({"$ref":"#/$defs/CompleteFormField"}), MAX_COLLECTION_ITEMS)
                }),
                &["purpose", "fields"],
            ),
        ),
        tagged_input(
            "submitAndVerify",
            object(
                json!({
                    "purpose":string(1, 256),
                    "hints":{"$ref":"#/$defs/IntentHints"},
                    "expectedState":{"$ref":"#/$defs/WaitForCommand"}
                }),
                &["purpose", "expectedState"],
            ),
        ),
        tagged_input(
            "waitForState",
            object(
                json!({
                    "condition":{"$ref":"#/$defs/WaitCondition"},
                    "timeoutMs":{"type":"integer","minimum":1,"maximum":MAX_TIMEOUT_MS}
                }),
                &["condition", "timeoutMs"],
            ),
        ),
        tagged_input(
            "follow",
            object(
                json!({
                    "purpose":string(1, 256),
                    "hints":{"$ref":"#/$defs/IntentHints"},
                    "expectedDestination":{"$ref":"#/$defs/WaitForCommand"},
                    "boundary":{"type":"boolean"}
                }),
                &["purpose", "expectedDestination"],
            ),
        ),
        tagged_input(
            "dismissObstruction",
            object(
                json!({
                    "purpose":string(1, 256),
                    "hints":{"$ref":"#/$defs/IntentHints"},
                    "timeoutMs":{"type":"integer","minimum":1,"maximum":MAX_TIMEOUT_MS}
                }),
                &["purpose"],
            ),
        ),
        tagged_input(
            "extract",
            object(
                json!({
                    "purpose":string(1, 256),
                    "fields":{
                        "type":"array",
                        "items":{"$ref":"#/$defs/ExtractField"},
                        "minItems":1,
                        "maxItems":MAX_COLLECTION_ITEMS
                    }
                }),
                &["purpose", "fields"],
            ),
        ),
        tagged_input(
            "solveChallenge",
            object(
                json!({
                    "purpose":string(1, 256),
                    "hints":{
                        "type":"object",
                        "properties":{
                            "timeoutMs":{"type":"integer","minimum":1,"maximum":MAX_TIMEOUT_MS},
                            "region":{
                                "type":"object",
                                "properties":{
                                    "x":{"type":"number"},
                                    "y":{"type":"number"},
                                    "width":{"type":"number"},
                                    "height":{"type":"number"}
                                },
                                "required":["x","y","width","height"],
                                "additionalProperties":false
                            }
                        },
                        "additionalProperties":false
                    }
                }),
                &["purpose"],
            ),
        ),
        tagged_input(
            "detectChallenge",
            object(
                json!({
                    "purpose":string(1, 256),
                    "hints":{
                        "type":"object",
                        "properties":{
                            "timeoutMs":{"type":"integer","minimum":1,"maximum":MAX_TIMEOUT_MS},
                            "region":{
                                "type":"object",
                                "properties":{
                                    "x":{"type":"number"},
                                    "y":{"type":"number"},
                                    "width":{"type":"number"},
                                    "height":{"type":"number"}
                                },
                                "required":["x","y","width","height"],
                                "additionalProperties":false
                            }
                        },
                        "additionalProperties":false
                    }
                }),
                &["purpose"],
            ),
        ),
    ]
}

fn extract_field() -> Value {
    object(
        json!({
            "name":string(1, 256),
            "purpose":string(1, 256),
            "hints":{"$ref":"#/$defs/IntentHints"},
            "value":{"$ref":"#/$defs/ExtractValueKind"}
        }),
        &["name", "purpose", "value"],
    )
}

fn extract_value_kinds() -> Vec<Value> {
    vec![
        tagged_fields("text", json!({}), &[]),
        tagged_fields(
            "attribute",
            json!({"attribute":string(1, 256)}),
            &["attribute"],
        ),
        tagged_fields("href", json!({}), &[]),
    ]
}

fn intent_hints() -> Value {
    object(
        json!({
            "role":nullable(string(0, 256)),
            "accessibleName":nullable(string(0, 256)),
            "nearText":nullable(json!({"$ref":"#/$defs/TextMatch"})),
            "ordinal":nullable(json!({"type":"integer","minimum":0,"maximum":1000000})),
            "framePath":array(json!({"$ref":"#/$defs/TargetSpec"}), 16),
            "shadowPath":array(json!({"$ref":"#/$defs/TargetSpec"}), 16),
            "allowBestMatch":{"type":"boolean"}
        }),
        &[],
    )
}

/// The `ControlAction` subset a fill accepts: every variant but `activate`,
/// which control_action alone can express (see `crate::commands::ControlAction`
/// in `bobby-browser-client`; fill rejects it at the intent-engine layer).
fn fill_values() -> Vec<Value> {
    vec![
        tagged_fields(
            "setText",
            json!({
                "value":string(0, MAX_STRING_BYTES),
                "clearFirst":{
                    "type":"boolean",
                    "default":true,
                    "description":"replace the current value; set false to append"
                }
            }),
            &["value"],
        ),
        tagged_fields(
            "setChecked",
            json!({"checked":{"type":"boolean"}}),
            &["checked"],
        ),
        tagged_fields(
            "selectOne",
            json!({"value":string(0, MAX_STRING_BYTES)}),
            &["value"],
        ),
        tagged_fields(
            "selectMany",
            json!({"values":nonempty_array(string(0, MAX_STRING_BYTES), types::MAX_FORM_REFERENCES)}),
            &["values"],
        ),
        tagged_fields(
            "setFiles",
            json!({"paths":nonempty_array(string(1, MAX_STRING_BYTES), types::MAX_FORM_REFERENCES)}),
            &["paths"],
        ),
        tagged_fields("clear", json!({}), &[]),
    ]
}

fn complete_form_field() -> Value {
    object(
        json!({
            "name":string(1, MAX_STRING_BYTES),
            "purpose":string(1, 256),
            "hints":{"$ref":"#/$defs/IntentHints"},
            "value":{"$ref":"#/$defs/FillValue"}
        }),
        &["name", "purpose", "value"],
    )
}

fn wait_for_command() -> Value {
    object(
        json!({
            "condition":{"$ref":"#/$defs/WaitCondition"},
            "timeoutMs":{"type":"integer","minimum":1,"maximum":MAX_TIMEOUT_MS}
        }),
        &["condition", "timeoutMs"],
    )
}

fn execution_record() -> Value {
    object(
        json!({
            "intentKind":string(1, 128),
            "purpose":nullable(string(0, 256)),
            "resolutionPath":{"type":"string","enum":["deterministic","visionFallback"]},
            "planSummary":string(0, MAX_STRING_BYTES),
            "candidates":array(object(json!({
                "role":nullable(string(0,256)),
                "name":nullable(string(0,MAX_STRING_BYTES)),
                "score":{"type":"integer","minimum":-1000000,"maximum":1000000},
                "reasons":array(string(0,MAX_STRING_BYTES),64)
            }), &["role","name","score","reasons"]), 64),
            "waitElapsedMs":nullable(json!({"type":"integer","minimum":0,"maximum":MAX_TIMEOUT_MS})),
            "verification":string(0, MAX_STRING_BYTES),
            "artifactIds":array(string(1, 128), 64),
            "visionProposalSha256":nullable(sha256())
        }),
        &[
            "intentKind",
            "purpose",
            "resolutionPath",
            "planSummary",
            "candidates",
            "waitElapsedMs",
            "verification",
            "artifactIds",
            "visionProposalSha256",
        ],
    )
}

fn primitive_commands() -> Vec<Value> {
    vec![
        tagged_input(
            "navigate",
            object(
                json!({
                    "url":string(1, MAX_URL_BYTES),
                    "waitUntil":{"type":"string","enum":["commit","domContentLoaded","interactive","networkIdle"]},
                    "timeoutMs":{"type":"integer","minimum":1,"maximum":MAX_TIMEOUT_MS}
                }),
                &["url", "waitUntil", "timeoutMs"],
            ),
        ),
        tagged_input(
            "downloadUrl",
            object(
                json!({
                    "url":string(1, MAX_URL_BYTES), "expectedContentType":nullable(string(0, 256)),
                    "maxBytes":{"type":"integer","minimum":1,"maximum":1073741824u64}
                }),
                &["url", "expectedContentType", "maxBytes"],
            ),
        ),
        tagged_input(
            "inspect",
            object(
                json!({
                    "selector":nullable(string(0, MAX_STRING_BYTES)), "target":nullable(json!({"$ref":"#/$defs/TargetSpec"})),
                    "includeHtml":{"type":"boolean"}
                }),
                &["selector", "target", "includeHtml"],
            ),
        ),
        tagged_input(
            "click",
            object(
                json!({
                    "selector":string(0, MAX_STRING_BYTES), "target":nullable(json!({"$ref":"#/$defs/TargetSpec"})),
                    "boundary":{"type":"boolean"}, "expectedUrl":nullable(string(0, MAX_URL_BYTES))
                }),
                &["selector", "target", "boundary", "expectedUrl"],
            ),
        ),
        tagged_input(
            "typeText",
            object(
                json!({
                    "selector":string(0, MAX_STRING_BYTES), "target":nullable(json!({"$ref":"#/$defs/TargetSpec"})),
                    "value":string(0, MAX_STRING_BYTES), "clearFirst":{"type":"boolean"}
                }),
                &["selector", "target", "value", "clearFirst"],
            ),
        ),
        tagged_input(
            "uploadFiles",
            object(
                json!({
                    "selector":string(0, MAX_STRING_BYTES), "target":nullable(json!({"$ref":"#/$defs/TargetSpec"})),
                    "paths":array(string(1, MAX_STRING_BYTES), 64)
                }),
                &["selector", "target", "paths"],
            ),
        ),
        tagged_input(
            "controlAction",
            object(
                json!({
                    "target": {
                        "type":"object",
                        "additionalProperties":false,
                        "properties":{
                            "role":string(1,128),
                            "accessibleName":string(1,2048),
                            "ordinal":{"type":["integer","null"],"minimum":0,"maximum":2047},
                            "framePath":array(any_value(),8),
                            "shadowPath":array(any_value(),8)
                        },
                        "required":["role","accessibleName","ordinal","framePath","shadowPath"]
                    },
                    "action": {"oneOf":[
                        {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"setText"},"value":string(0,4096),"clearFirst":{"type":"boolean","default":true,"description":"replace the current value; set false to append"}},"required":["kind","value"]},
                        {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"setChecked"},"checked":{"type":"boolean"}},"required":["kind","checked"]},
                        {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"selectOne"},"value":string(0,4096)},"required":["kind","value"]},
                        {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"selectMany"},"values":nonempty_array(string(0,4096),512)},"required":["kind","values"]},
                        {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"setFiles"},"paths":nonempty_array(string(1,4096),512)},"required":["kind","paths"]},
                        {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"clear"}},"required":["kind"]},
                        {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"activate"}},"required":["kind"]}
                    ]}
                }),
                &["target", "action"],
            ),
        ),
        tagged_input(
            "openPage",
            object(json!({"url":nullable(string(0, MAX_URL_BYTES))}), &["url"]),
        ),
        tagged_input("listPages", json!({"type":"null"})),
        tagged_input("closePage", object(json!({"pageId":id()}), &["pageId"])),
        tagged_input("activatePage", object(json!({"pageId":id()}), &["pageId"])),
        tagged_input(
            "networkLog",
            object(json!({"clear":{"type":"boolean"}}), &[]),
        ),
        tagged_input(
            "emulate",
            object(
                json!({
                    "viewport":nullable(json!({"$ref":"#/$defs/ViewportSize"})),
                    "geolocation":nullable(json!({"$ref":"#/$defs/GeolocationCoordinates"})),
                    "mobile":{"type":"boolean"}
                }),
                &[],
            ),
        ),
        tagged_input(
            "handleDialog",
            object(
                json!({
                    "action":{"type":"string","enum":["accept","dismiss"]},
                    "timeoutMs":{"type":"integer","minimum":1,"maximum":MAX_TIMEOUT_MS}
                }),
                &["action"],
            ),
        ),
        tagged_input(
            "printToPdf",
            object(
                json!({
                    "landscape":{"type":"boolean"},
                    "printBackground":{"type":"boolean"},
                    "scale":{"type":"number","minimum":0.1,"maximum":2.0},
                    "pageRanges":nullable(string(0, 256))
                }),
                &[],
            ),
        ),
        tagged_input(
            "getCookies",
            object(json!({"urls":array(string(1, MAX_URL_BYTES), 64)}), &[]),
        ),
        tagged_input(
            "setCookies",
            object(
                json!({"cookies":array(json!({"$ref":"#/$defs/SetCookieParam"}), 128)}),
                &["cookies"],
            ),
        ),
        tagged_input(
            "deleteCookies",
            object(
                json!({
                    "urls":array(string(1, MAX_URL_BYTES), 64),
                    "names":array(string(1, 1024), 128)
                }),
                &[],
            ),
        ),
        tagged_input(
            "accessibilitySnapshot",
            object(
                json!({"maxNodes":{"type":"integer","minimum":1,"maximum":2048}}),
                &[],
            ),
        ),
        tagged_input(
            "extractStructured",
            object(
                json!({"schema":any_value(),"purpose":nullable(string(1, 256))}),
                &["schema"],
            ),
        ),
        tagged_input("clickAndWaitForPopup", click_wait_input()),
        tagged_input("clickAndWaitForDownload", click_wait_input()),
        tagged_input(
            "waitFor",
            object(
                json!({
                    "condition":{"$ref":"#/$defs/WaitCondition"},
                    "timeoutMs":{"type":"integer","minimum":1,"maximum":MAX_TIMEOUT_MS}
                }),
                &["condition", "timeoutMs"],
            ),
        ),
        tagged_input(
            "captureScreenshot",
            object(json!({"mode":{"$ref":"#/$defs/ScreenshotMode"}}), &["mode"]),
        ),
        tagged_input(
            "setFocusEmulation",
            object(json!({"enabled":{"type":"boolean"}}), &["enabled"]),
        ),
        tagged_input(
            "setEmulatedMedia",
            object(
                json!({
                    "media":string(0, 256),
                    "features":{"type":"object","maxProperties":64,"propertyNames":{"maxLength":128},"additionalProperties":string(0, MAX_STRING_BYTES)}
                }),
                &["media", "features"],
            ),
        ),
        tagged_input(
            "evaluateJavaScript",
            object(
                json!({
                    "expression":string(1, MAX_STRING_BYTES),
                    "timeoutMs":{"type":"integer","minimum":1,"maximum":MAX_TIMEOUT_MS},
                    "awaitPromise":{"type":"boolean"}
                }),
                &["expression", "timeoutMs", "awaitPromise"],
            ),
        ),
    ]
}

fn click_wait_input() -> Value {
    object(
        json!({
            "selector":string(0, MAX_STRING_BYTES), "target":nullable(json!({"$ref":"#/$defs/TargetSpec"})),
            "timeoutMs":{"type":"integer","minimum":1,"maximum":MAX_TIMEOUT_MS}
        }),
        &["selector", "target", "timeoutMs"],
    )
}

fn target_spec() -> Value {
    object(
        json!({
            "css":nullable(string(0, MAX_STRING_BYTES)), "testId":nullable(string(0, MAX_STRING_BYTES)),
            "role":nullable(string(0, 256)), "accessibleName":nullable(string(0, MAX_STRING_BYTES)),
            "label":nullable(string(0, MAX_STRING_BYTES)), "text":nullable(json!({"$ref":"#/$defs/TextMatch"})),
            "attributes":{"type":"object","maxProperties":64,"propertyNames":{"maxLength":128},"additionalProperties":string(0, MAX_STRING_BYTES)},
            "framePath":array(json!({"$ref":"#/$defs/TargetSpec"}), 16),
            "shadowPath":array(json!({"$ref":"#/$defs/TargetSpec"}), 16),
            "ordinal":nullable(json!({"type":"integer","minimum":0,"maximum":1000000})),
            "allowBestMatch":{"type":"boolean"}
        }),
        &[],
    )
}

fn wait_conditions() -> Vec<Value> {
    vec![
        tagged_fields(
            "element",
            json!({"target":{"$ref":"#/$defs/TargetSpec"},"state":{"type":"string","enum":["attached","detached","visible","hidden","enabled","disabled"]}}),
            &["target", "state"],
        ),
        tagged_fields(
            "text",
            json!({"target":{"$ref":"#/$defs/TargetSpec"},"matcher":{"$ref":"#/$defs/TextMatch"}}),
            &["target", "matcher"],
        ),
        tagged_fields(
            "value",
            json!({"target":{"$ref":"#/$defs/TargetSpec"},"matcher":{"$ref":"#/$defs/TextMatch"}}),
            &["target", "matcher"],
        ),
        tagged_fields(
            "url",
            json!({"matcher":{"$ref":"#/$defs/TextMatch"}}),
            &["matcher"],
        ),
        tagged_fields(
            "document",
            json!({"ready":{"type":"string","enum":["commit","domContentLoaded","interactive","networkIdle"]}}),
            &["ready"],
        ),
        tagged_fields(
            "networkQuiet",
            json!({
                "idleMs":{"type":"integer","minimum":0,"maximum":MAX_TIMEOUT_MS},
                "maxInFlight":{"type":"integer","minimum":0,"maximum":10000},
                "ignoreUrlSubstrings":array(string(1, MAX_NETWORK_IGNORE_SUBSTRING_BYTES), MAX_NETWORK_IGNORE_SUBSTRINGS),
                "ignoreResourceTypes":array(network_resource_type(), MAX_NETWORK_IGNORE_RESOURCE_TYPES),
                "ignoreLongLived":{"type":"boolean"}
            }),
            &["idleMs", "maxInFlight"],
        ),
    ]
}

fn network_resource_type() -> Value {
    json!({
        "type":"string",
        "enum":[
            "Document","Stylesheet","Image","Media","Font","Script","TextTrack","XHR","Fetch",
            "Prefetch","EventSource","WebSocket","Manifest","SignedExchange","Ping",
            "CSPViolationReport","Preflight","FedCM","Other"
        ]
    })
}

fn screenshot_modes() -> Vec<Value> {
    vec![
        object(json!({"kind":{"const":"viewport"}}), &["kind"]),
        object(json!({"kind":{"const":"fullPage"}}), &["kind"]),
        tagged_fields(
            "element",
            json!({"target":{"$ref":"#/$defs/TargetSpec"}}),
            &["target"],
        ),
        tagged_fields(
            "clip",
            json!({
                "x":finite_number(),"y":finite_number(),"width":positive_number(),"height":positive_number()
            }),
            &["x", "y", "width", "height"],
        ),
    ]
}

fn evidence_variants() -> Vec<Value> {
    vec![
        tagged_fields(
            "executionPath",
            json!({
                "path":{"type":"string","enum":["directHttp","chromium","chromiumFallback"]},
                "reason":{"type":"string","enum":["eligibleStaticDocument","eligibleExplicitDownload","ineligibleCommand","semanticTargetRequired","javascriptRequired","unsupportedContentType","stateConflict","policyRequired","pageMutated"],"description":"Why the direct-HTTP fast path was or was not taken; ineligibleCommand means the command class runs in the browser and is not a failure."},
                "stateVersion":{"type":"integer","minimum":0},"elapsedMs":{"type":"integer","minimum":0},
                "bytes":nullable(json!({"type":"integer","minimum":0})),"sha256":nullable(sha256()),
                "finalUrl":nullable(string(0, MAX_URL_BYTES)),"contentType":nullable(string(0,256)),
                "status":nullable(json!({"type":"integer","minimum":100,"maximum":599})),
                "redirectChain":array(string(0, MAX_URL_BYTES), 32)
            }),
            &[
                "path",
                "reason",
                "stateVersion",
                "elapsedMs",
                "bytes",
                "sha256",
            ],
        ),
        tagged_fields(
            "navigation",
            json!({"url":string(0,MAX_URL_BYTES),"title":string(0,MAX_STRING_BYTES)}),
            &["url", "title"],
        ),
        tagged_fields(
            "inspection",
            json!({"selector":nullable(string(0,MAX_STRING_BYTES)),"url":string(0,MAX_URL_BYTES),"title":string(0,MAX_STRING_BYTES),"text":string(0,MAX_STRING_BYTES),"html":nullable(string(0,MAX_HTML_BYTES))}),
            &["selector", "url", "title", "text", "html"],
        ),
        tagged_fields(
            "submitSettlement",
            json!({"outcome":{"type":"string","enum":["settled","validationRejected"]}}),
            &["outcome"],
        ),
        tagged_fields(
            "element",
            json!({"selector":string(0,MAX_STRING_BYTES),"text":nullable(string(0,MAX_STRING_BYTES))}),
            &["selector", "text"],
        ),
        tagged_fields(
            "upload",
            json!({"selector":string(0,MAX_STRING_BYTES),"paths":array(string(1,MAX_STRING_BYTES),64)}),
            &["selector", "paths"],
        ),
        tagged_fields(
            "page",
            page_evidence_properties(),
            &["pageId", "url", "title"],
        ),
        tagged_fields(
            "pages",
            json!({"pages":array(object(page_evidence_properties(), &["pageId","url","title"]),MAX_COLLECTION_ITEMS)}),
            &["pages"],
        ),
        tagged_fields(
            "popup",
            json!({"openerPageId":id(),"pageId":id(),"url":string(0,MAX_URL_BYTES),"title":string(0,MAX_STRING_BYTES)}),
            &["openerPageId", "pageId", "url", "title"],
        ),
        tagged_fields(
            "download",
            json!({"filename":string(1,MAX_STRING_BYTES),"path":string(1,MAX_STRING_BYTES),"bytes":{"type":"integer","minimum":0},"sha256":sha256(),"savedTo":string(1,4096)}),
            &["filename", "path", "bytes", "sha256"],
        ),
        tagged_fields(
            "resolution",
            json!({
                "target":{"$ref":"#/$defs/TargetSpec"},
                "fingerprint":object(json!({"pageId":id(),"frame":nullable(string(0,MAX_STRING_BYTES)),"role":nullable(string(0,256)),"name":nullable(string(0,MAX_STRING_BYTES)),"stableAttributes":{"type":"object","maxProperties":64,"propertyNames":{"maxLength":128},"additionalProperties":string(0,MAX_STRING_BYTES)}}), &["pageId","frame","role","name","stableAttributes"]),
                "candidates":array(object(json!({"role":nullable(string(0,256)),"name":nullable(string(0,MAX_STRING_BYTES)),"score":{"type":"integer","minimum":-1000000,"maximum":1000000},"reasons":array(string(0,MAX_STRING_BYTES),64)}), &["role","name","score","reasons"]),64),
                "bestMatchAuthorized":{"type":"boolean"}
            }),
            &["target", "fingerprint", "candidates", "bestMatchAuthorized"],
        ),
        tagged_fields(
            "wait",
            json!({
                "condition":{"$ref":"#/$defs/WaitCondition"},
                "elapsedMs":{"type":"integer","minimum":0},
                "observations":{"type":"integer","minimum":0},
                "excludedClasses":array(string(1, MAX_STRING_BYTES), MAX_EXCLUDED_CLASSES),
                "observed":string(0, types::MAX_WAIT_OBSERVED_CHARS)
            }),
            &["condition", "elapsedMs", "observations"],
        ),
        tagged_fields(
            "screenshot",
            json!({"artifactId":string(1,128),"mediaType":string(1,256),"width":{"type":"integer","minimum":1,"maximum":16384},"height":{"type":"integer","minimum":1,"maximum":16384},"bytes":{"type":"integer","minimum":1,"maximum":1073741824u64},"sha256":sha256()}),
            &[
                "artifactId",
                "mediaType",
                "width",
                "height",
                "bytes",
                "sha256",
            ],
        ),
        tagged_fields(
            "configuration",
            json!({"name":string(0,MAX_STRING_BYTES),"value":string(0,MAX_STRING_BYTES)}),
            &["name", "value"],
        ),
        tagged_fields(
            "browserExecution",
            json!({
                "engine":string(0,MAX_STRING_BYTES),
                "browserVersion":string(0,MAX_STRING_BYTES),
                "profileId":string(0,MAX_STRING_BYTES),
                "interactionPath":string(0,MAX_STRING_BYTES)
            }),
            &["engine", "browserVersion", "profileId", "interactionPath"],
        ),
        tagged_fields(
            "javaScriptResult",
            json!({"value":any_value(),"truncated":{"type":"boolean"}}),
            &["value", "truncated"],
        ),
        tagged_fields(
            "accessibilitySnapshot",
            json!({
                "pageId":id(),
                "nodes":array(json!({"$ref":"#/$defs/AccessibilityNode"}), 2048),
                "truncated":{"type":"boolean"}
            }),
            &["pageId", "nodes", "truncated"],
        ),
        tagged_fields(
            "formSnapshot",
            json!({"snapshot":any_value()}),
            &["snapshot"],
        ),
        tagged_fields(
            "formValidation",
            json!({"issues":array(json!({"$ref":"#/$defs/FormValidationIssue"}), 512)}),
            &["issues"],
        ),
        tagged_fields("controlAction", json!({"action":any_value()}), &["action"]),
        tagged_fields(
            "emulation",
            json!({
                "viewport":nullable(json!({"$ref":"#/$defs/ViewportSize"})),
                "geolocation":nullable(json!({"$ref":"#/$defs/GeolocationCoordinates"}))
            }),
            &[],
        ),
        tagged_fields(
            "dialog",
            json!({
                "dialogType":string(1, 64),
                "message":string(0, MAX_STRING_BYTES),
                "action":{"type":"string","enum":["accept","dismiss"]}
            }),
            &["dialogType", "message", "action"],
        ),
        tagged_fields(
            "harArtifact",
            json!({
                "artifactId":string(1, 128),
                "mediaType":string(1, 256),
                "bytes":{"type":"integer","minimum":1,"maximum":16777216},
                "sha256":sha256(),
                "entries":{"type":"integer","minimum":0,"maximum":512}
            }),
            &["artifactId", "mediaType", "bytes", "sha256", "entries"],
        ),
        tagged_fields(
            "pdfArtifact",
            json!({
                "artifactId":string(1, 128),
                "mediaType":string(1, 256),
                "bytes":{"type":"integer","minimum":1,"maximum":16777216},
                "sha256":sha256()
            }),
            &["artifactId", "mediaType", "bytes", "sha256"],
        ),
        tagged_fields(
            "cookieState",
            json!({
                "pageId":nullable(id()),
                "cookies":array(json!({"$ref":"#/$defs/CookieRecord"}), 2048)
            }),
            &["pageId", "cookies"],
        ),
        tagged_fields(
            "structuredExtraction",
            json!({"pageId":id(),"value":any_value(),"truncated":{"type":"boolean"}}),
            &["pageId", "value", "truncated"],
        ),
        // `Evidence::ChallengeDetection`: the enum camelCases its own variant
        // fields (`confidence`, `detection`, `priorKind`); the inner
        // ChallengeDetection type carries no rename, so its fields stay
        // snake_case.
        tagged_fields(
            "challengeDetection",
            json!({
                "confidence":{"type":"number"},
                "detection":nullable(object(
                    json!({
                        "challenge_type":{"enum":[
                            "recaptchaV2Checkbox",
                            "recaptchaV3",
                            "textCaptcha",
                            "imageGridCaptcha",
                            "mfaCodeEntry"
                        ]},
                        "confidence":{"type":"number"},
                        "region":object(
                            json!({
                                "x":{"type":"number"},
                                "y":{"type":"number"},
                                "width":{"type":"number"},
                                "height":{"type":"number"}
                            }),
                            &["x","y","width","height"]
                        ),
                        "blocking":{"type":"boolean"},
                        "hints":object(
                            json!({
                                "target_field_purpose":string(0, 1024),
                                "instruction_text":string(0, 4096)
                            }),
                            &[]
                        )
                    }),
                    &["challenge_type", "confidence", "blocking"]
                )),
                "priorKind":string(1, 128)
            }),
            &["confidence", "detection"],
        ),
        tagged_fields(
            "intentExecution",
            json!({"record":{"$ref":"#/$defs/ExecutionRecord"}}),
            &["record"],
        ),
        tagged_fields(
            "humanization",
            json!({
                "engine":string(1, 64),
                "actions":{"type":"integer","minimum":0,"maximum":65535},
                "synthesizedMs":{"type":"integer","minimum":0,"maximum":600000}
            }),
            &["engine", "actions", "synthesizedMs"],
        ),
        tagged_fields(
            "extraction",
            json!({
                "field":string(1, 256),
                "value":nullable(string(0, MAX_STRING_BYTES)),
                "resolutionPath":{"type":"string","enum":["deterministic","visionFallback"]},
                "errorCode":nullable(error_code())
            }),
            &["field", "value", "resolutionPath", "errorCode"],
        ),
    ]
}

/// Must match `types::ErrorCode`'s camelCase serde output variant-for-variant.
pub(crate) fn error_code_for_test() -> Value {
    error_code()
}

fn error_code() -> Value {
    json!({"type":"string","enum":[
        "invalidRequest","notFound","deadlineExceeded","browserLaunchFailed",
        "browserCommandFailed","verificationFailed","journalFailed","resourceExhausted",
        "policyDenied","internal","targetNotFound","targetAmbiguous","frameNotFound",
        "shadowRootUnavailable","targetDetached","waitConditionTimedOut",
        "screenshotCaptureFailed","networkPolicyDenied","httpResponseTooLarge",
        "httpTransferFailed","httpStateConflict","httpEquivalenceUnproven",
        "intentCompileFailed","intentActionMismatch","obstructionSuspected",
        "visionAssistDenied","visionAssistFailed","targetObscured","targetOutOfBounds",
        "expectedStatePreSatisfied"
    ]})
}

fn page_evidence_properties() -> Value {
    json!({"pageId":id(),"url":string(0,MAX_URL_BYTES),"title":string(0,MAX_STRING_BYTES)})
}

fn recovery_decisions() -> Vec<Value> {
    vec![
        status_fields(
            "resumed",
            json!({"checkpointId":id(),"attemptId":id(),"evidence":array(json!({"$ref":"#/$defs/Evidence"}),MAX_EVIDENCE_ITEMS)}),
            &["checkpointId", "attemptId", "evidence"],
        ),
        status_fields(
            "needsReconciliation",
            json!({"checkpointId":id(),"attemptId":id(),"reason":string(1,MAX_STRING_BYTES),"evidence":array(json!({"$ref":"#/$defs/Evidence"}),MAX_EVIDENCE_ITEMS)}),
            &["checkpointId", "attemptId", "reason", "evidence"],
        ),
        status_fields(
            "restarted",
            json!({
                "checkpointId":id(),
                "lineage":object(json!({"workflowId":id(),"abandonedAttemptId":id(),"attemptId":id(),"reason":string(1,MAX_STRING_BYTES)}), &["workflowId","abandonedAttemptId","attemptId","reason"]),
                // `RecoveryDecision::Restarted.evidence` has `#[serde(default)]` for
                // deserialization only; it carries no `skip_serializing_if`, so the
                // runtime always serializes it (possibly empty), same as the other
                // two variants.
                "evidence":array(json!({"$ref":"#/$defs/Evidence"}),MAX_EVIDENCE_ITEMS)
            }),
            &["checkpointId", "lineage", "evidence"],
        ),
    ]
}

// --- Output-only schema helpers -------------------------------------------
//
// Everything below describes `structuredContent`, never a tool argument. Only
// `tool_output_schema` reads these.

/// A flattened, self-contained approximation of `types::CommandOutcome` (`tag =
/// "status"`, 7 variants): every field any variant carries, at the top level, all
/// optional except `status`/`commandId`. Inlined rather than shared as a named
/// definition because the fully-typed union costs ~3.8 KB and ~31 tools would each
/// carry it, which alone exceeds the connect budget. `error`/`evidence` stay generic
/// objects; full `Evidence` fidelity lives only on `workflow_recover`'s
/// `RecoveryDecision`.
///
/// `object()` closes the schema (`additionalProperties:false`), so every field a real
/// `CommandOutcome` can carry must be declared here, including `priorAttemptId` and
/// `attemptId` (optional, only `status: "restarted"` carries them).
///
/// `artifactRegistration` is not part of `types::CommandOutcome`: `submit_envelope`
/// inserts it via `ArtifactAdmission::apply_to_mcp_value` whenever screenshot or
/// download evidence did not fully admit. Kept generic for the same byte reason.
fn command_outcome_properties() -> Value {
    json!({
        "status":{"type":"string","enum":["completed","retryableFailure","needsReconciliation","policyDenied","resourceExhausted","restarted","failed"]},
        "commandId":id(),
        "evidence":{"type":"array","items":{"type":"object"}},
        "error":{"type":"object"},
        "retryAfterMs":{"type":"integer","minimum":0},
        "reason":string(0, MAX_STRING_BYTES),
        "priorAttemptId":id(),
        "attemptId":id(),
        "artifactRegistration":{"type":"object"}
    })
}

/// A byte-frugal projection of [`evidence_variants()`]: every `kind` tag it serializes,
/// none of its fields. A field-by-field replica puts `tools/list` past the 128 KiB frame
/// budget (`tools_list_stays_within_the_connect_budget`, `tests/budget.rs`) as soon as a
/// single tool reaches it.
///
/// `types::Evidence` is `#[serde(tag = "kind")]`, so real fields sit flat next to `kind`
/// and never under a `data` key. The schema must therefore stay OPEN: declare and require
/// `kind` only, leave `additionalProperties` unset, so real fields pass through. A closed
/// `{kind, data}` shape rejects every real evidence object.
///
/// Derived from [`evidence_variants()`] rather than a second hand-written tag list, which
/// would drift.
fn evidence_variant_tags() -> Vec<Value> {
    evidence_variants()
        .into_iter()
        .map(|variant| {
            let kind = variant["properties"]["kind"]["const"]
                .as_str()
                .expect("evidence variant pins a kind const")
                .to_owned();
            json!({
                "type":"object",
                "properties":{"kind":{"const":kind}},
                "required":["kind"]
            })
        })
        .collect()
}

/// Status-tag projection of [`recovery_decisions()`] for the advertised
/// `workflow_recover` output schema, mirroring [`evidence_variant_tags`]:
/// the fully-fielded union plus its reachable `Evidence` tags costs ~3.3 KB
/// on a single tool entry. Derived, not hand-written, so it cannot drift.
fn recovery_decision_tags() -> Vec<Value> {
    recovery_decisions()
        .into_iter()
        .map(|variant| {
            let status = variant["properties"]["status"]["const"]
                .as_str()
                .expect("recovery decision pins a status const")
                .to_owned();
            json!({
                "type":"object",
                "properties":{"status":{"const":status}},
                "required":["status"]
            })
        })
        .collect()
}

/// Must match `types::SessionState`. The struct itself carries no
/// `rename_all`, so these keys are snake_case; only the nested
/// `execution_policy` is camelCase, since `types::ExecutionPolicy` sets its
/// own `rename_all = "camelCase"`.
fn session_state() -> Value {
    object(
        json!({
            "id":id(),
            "profile":string(1, 128),
            "proxy":nullable(string(0, 2048)),
            "page_ids":array(id(), MAX_COLLECTION_ITEMS),
            "created_at":{"type":"string","format":"date-time","minLength":20,"maxLength":64},
            "last_used_at":{"type":"string","format":"date-time","minLength":20,"maxLength":64},
            "execution_policy":object(
                json!({
                    "javascriptEvaluation":{"type":"boolean"},
                    "visionAssist":{"type":"boolean"},
                    "fingerprint":{"type":"boolean"},
                    "humanize":{"type":"boolean"},
                    "visionNode":string(1, 128)
                }),
                &[]
            ),
            // Present iff true (`skip_serializing_if`), so optional here is
            // exact, and the godmode bit costs no schema bytes when off.
            "zigzagzig":{"type":"boolean"}
        }),
        &[
            "id",
            "profile",
            "proxy",
            "page_ids",
            "created_at",
            "last_used_at",
            "execution_policy",
        ],
    )
}

/// Must match `types::PageState`. Also no `rename_all`, so snake_case keys;
/// `mode` serializes as the bare Rust variant name because `PageMode` has no
/// `rename_all` of its own.
fn page_state() -> Value {
    object(
        json!({
            "id":id(),
            "session_id":id(),
            "url":nullable(string(0, MAX_URL_BYTES)),
            "mode":{"type":"string","enum":["Document","Interactive","Render"]},
            "ready_state":string(0, 256),
            "pending_requests":{"type":"integer","minimum":0}
        }),
        &[
            "id",
            "session_id",
            "url",
            "mode",
            "ready_state",
            "pending_requests",
        ],
    )
}

/// The persisted `types::WorkflowCheckpoint` as `checkpoint_save` and `recovery_status`
/// return it. Distinct from the input definition (`workflow_checkpoint()`), which forces
/// `evidence`/`recoveryHistory`/`recoveryReceipts` empty; the runtime populates all three.
fn checkpoint_record() -> Value {
    object(
        json!({
            "schemaVersion":{"type":"integer","const":1},
            "checkpointId":id(), "workflowId":id(), "attemptId":id(), "sessionId":id(), "pageId":id(),
            "restartUrl":string(1, MAX_URL_BYTES), "currentUrl":string(1, MAX_URL_BYTES),
            "cursor":nullable(id()), "boundaryCommandId":nullable(id()),
            "recoveryClass":{"type":"string","enum":["replayable","reconciliable","boundary"]},
            "invariants":array(json!({"$ref":"#/$defs/CheckpointInvariant"}), MAX_COLLECTION_ITEMS),
            "replayableInputs":array(string(0, MAX_STRING_BYTES), MAX_COLLECTION_ITEMS),
            // Generic rather than `Evidence`/`RecoveryRecord` `$ref`s: those pull the
            // accessibility and form-control subsystems into both `checkpoint_save`
            // and `recovery_status`'s `$defs`.
            "evidence":array(
                json!({"type":"object","required":["kind"],"properties":{"kind":{"type":"string"}}}),
                MAX_EVIDENCE_ITEMS
            ),
            "recoveryHistory":array(
                json!({
                    "type":"object",
                    "required":["recordedAt","decision"],
                    "properties":{
                        "recordedAt":{"type":"string","format":"date-time"},
                        "decision":{"type":"object"}
                    }
                }),
                MAX_COLLECTION_ITEMS
            ),
            // `RecoveryReceipt` also carries a `CommandOutcome`, a `SkillOutcome`,
            // and a `SkillDecision`, none modeled here; kept generic to avoid
            // dragging in a third type subsystem.
            "recoveryReceipts":array(json!({"type":"object"}), MAX_RECOVERY_RECEIPTS),
            "createdAt":{"type":"string","format":"date-time","minLength":20,"maxLength":64}
        }),
        &[
            "schemaVersion",
            "checkpointId",
            "workflowId",
            "attemptId",
            "sessionId",
            "pageId",
            "restartUrl",
            "currentUrl",
            "cursor",
            "boundaryCommandId",
            "recoveryClass",
            "invariants",
            "replayableInputs",
            "evidence",
            "recoveryHistory",
            "recoveryReceipts",
            "createdAt",
        ],
    )
}

/// Must match `types::FormSnapshot`'s wire representation (`FormSnapshotWire`).
fn form_snapshot_schema() -> Value {
    object(
        json!({
            "schemaVersion":{"type":"integer","const":1},
            "pageId":id(),
            "forms":array(json!({"$ref":"#/$defs/FormDescriptor"}), 64),
            "unownedControls":array(json!({"$ref":"#/$defs/FormControl"}), 512),
            "truncated":{"type":"boolean"}
        }),
        &[
            "schemaVersion",
            "pageId",
            "forms",
            "unownedControls",
            "truncated",
        ],
    )
}

/// Must match `types::FormDescriptor`. `groups`/`validity` stay generic rather than
/// `FormGroup`/`FormValidity` `$ref`s: `form_snapshot` is the most expensive tool
/// descriptor in the connect budget, and `FormControl` already surfaces the same fields.
fn form_descriptor() -> Value {
    object(
        json!({
            "id":string(1, 128),
            "target":nullable(json!({"$ref":"#/$defs/FormControlTarget"})),
            "accessibleName":nullable(string(0, 2048)),
            "description":nullable(string(0, 2048)),
            "groups":array(json!({"type":"object"}), 128),
            "controls":array(json!({"$ref":"#/$defs/FormControl"}), 512),
            "submitControlIds":array(string(1, 128), 512),
            "resetControlIds":array(string(1, 128), 512),
            "validity":{"type":"object"}
        }),
        &[
            "id",
            "target",
            "accessibleName",
            "description",
            "groups",
            "controls",
            "submitControlIds",
            "resetControlIds",
            "validity",
        ],
    )
}

/// Must match `types::FormControl`.
fn form_control() -> Value {
    object(
        json!({
            "id":string(1, 128),
            "formId":nullable(string(1, 128)),
            "groupId":nullable(string(1, 128)),
            "target":nullable(json!({"$ref":"#/$defs/FormControlTarget"})),
            "controlKind":{"type":"string","enum":[
                "text","email","password","search","number","checkbox","radio","switch",
                "selectOne","selectMultiple","date","time","dateTimeLocal","range","file",
                "contentEditable","combobox","listbox","submit","reset","other"
            ]},
            "accessibleName":nullable(string(0, 2048)),
            "label":nullable(string(0, 2048)),
            "description":nullable(string(0, 2048)),
            "placeholder":nullable(string(0, 2048)),
            "autocomplete":nullable(string(0, 2048)),
            "state":{"$ref":"#/$defs/FormControlState"},
            // Generic rather than `$ref:"#/$defs/FormControlConstraints"`: the
            // 11-field constraint object costs bytes that `controlKind` and
            // `validity`, both kept fully typed, already earn.
            "constraints":{"type":"object"},
            "validity":{"$ref":"#/$defs/FormControlValidity"},
            "options":array(json!({"$ref":"#/$defs/FormOption"}), 512),
            "supportedOperations":array(
                json!({"type":"string","enum":["setText","setChecked","selectOne","selectMany","setFiles","clear","activate"]}),
                7
            )
        }),
        &[
            "id",
            "formId",
            "groupId",
            "target",
            "controlKind",
            "accessibleName",
            "label",
            "description",
            "placeholder",
            "autocomplete",
            "state",
            "constraints",
            "validity",
            "options",
            "supportedOperations",
        ],
    )
}

/// Must match `types::FormValidationIssue`.
fn form_validation_issue() -> Value {
    object(
        json!({
            "controlId":string(1, 128),
            "controlKind":{"type":"string","enum":[
                "text","email","password","search","number","checkbox","radio","switch",
                "selectOne","selectMultiple","date","time","dateTimeLocal","range","file",
                "contentEditable","combobox","listbox","submit","reset","other"
            ]},
            "accessibleName":nullable(string(0, 2048)),
            "target":nullable(json!({"$ref":"#/$defs/FormControlTarget"})),
            "validity":{"$ref":"#/$defs/FormControlValidity"}
        }),
        &[
            "controlId",
            "controlKind",
            "accessibleName",
            "target",
            "validity",
        ],
    )
}

/// Must match `types::FormControlTarget`. `framePath`/`shadowPath` (arrays of
/// `SemanticTargetSegment`) stay generic rather than a `$ref`: rarely populated, and the
/// same three scalar fields as `role`/`accessibleName`/`ordinal` here.
/// Must match `types::FormControlTarget`. Only `role`/`accessibleName` are
/// required: an a11y snapshot's `target` passes verbatim, with ordinal and
/// the frame/shadow hops defaulting to empty. `framePath`/`shadowPath`
/// (arrays of `SemanticTargetSegment`) stay generic rather than a `$ref`:
/// rarely populated, and the same three scalar fields as here.
fn form_control_target() -> Value {
    object(
        json!({
            "role":string(0, 128),
            "accessibleName":string(0, MAX_STRING_BYTES),
            "ordinal":nullable(json!({"type":"integer","minimum":0})),
            "framePath":array(json!({"type":"object"}), 8),
            "shadowPath":array(json!({"type":"object"}), 8)
        }),
        &["role", "accessibleName"],
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::annotations::{tool_annotations, tool_title};
    use crate::tool_meta::{
        required_capabilities, required_operation, tool_description, WORKFLOW_OBSERVE_OPERATION,
        WORKFLOW_OBSERVE_REQUIRED_CAPABILITIES, WORKFLOW_START_OPERATION,
        WORKFLOW_START_REQUIRED_CAPABILITIES,
    };
    use crate::workflow_handles::WORKFLOW_SCOPE_TOOLS;

    #[test]
    fn click_modifier_schema_rejects_duplicates() {
        let violation = validate_tool_arguments(
            "click",
            &json!({
                "sessionId": "10000000-0000-4000-8000-000000000001",
                "pageId": "10000000-0000-4000-8000-000000000002",
                "selector": "#range-end",
                "modifiers": ["shift", "shift"]
            }),
        )
        .expect_err("duplicate modifiers must fail schema validation");
        assert_eq!(violation.pointer, "/modifiers");
        assert_eq!(violation.constraint, "uniqueItems");
    }

    #[test]
    fn workflow_contract_schemas_bound_inputs_and_define_object_outputs() {
        let start = tool_schema("workflow_start");
        assert_eq!(start["required"], json!(["profile"]));
        assert_eq!(start["properties"]["url"]["maxLength"], MAX_URL_BYTES);

        let observe = tool_schema("workflow_observe");
        assert_eq!(observe["required"], json!(["workflowHandle"]));
        assert_eq!(observe["properties"]["goal"]["maxLength"], 1024);
        assert_eq!(observe["properties"]["maxNodes"]["maximum"], 2048);
        assert_eq!(observe["properties"]["maxControls"]["maximum"], 512);

        for name in ["workflow_start", "workflow_observe"] {
            assert!(tool_title(name) != "Untitled tool", "{name}");
            assert_ne!(tool_description(name), "Runtime operation.", "{name}");
            assert!(tool_annotations(name).is_object(), "{name}");
            assert!(tool_schema(name).is_object(), "{name}");
            assert_eq!(tool_output_schema(name)["type"], "object", "{name}");
            jsonschema::validator_for(&tool_schema(name)).expect("input schema compiles");
            jsonschema::validator_for(&tool_output_schema(name)).expect("output schema compiles");
        }
        assert_eq!(
            required_capabilities("workflow_start"),
            Some(WORKFLOW_START_REQUIRED_CAPABILITIES)
        );
        assert_eq!(
            required_operation("workflow_start"),
            Some(WORKFLOW_START_OPERATION)
        );
        assert_eq!(
            required_capabilities("workflow_observe"),
            Some(WORKFLOW_OBSERVE_REQUIRED_CAPABILITIES)
        );
        assert_eq!(
            required_operation("workflow_observe"),
            Some(WORKFLOW_OBSERVE_OPERATION)
        );

        assert_eq!(
            tool_annotations("workflow_start"),
            json!({
                "readOnlyHint":false,
                "destructiveHint":false,
                "idempotentHint":false,
                "openWorldHint":true,
            })
        );
        assert_eq!(
            tool_annotations("workflow_observe"),
            json!({
                "readOnlyHint":true,
                "destructiveHint":false,
                "idempotentHint":false,
                "openWorldHint":false,
            })
        );
        let advertised_observe = advertised_tool_output_schema("workflow_observe");
        assert_eq!(
            advertised_observe["$defs"]["FormSnapshot"]["properties"]["forms"]["items"],
            json!({"type":"object"}),
        );
        assert_eq!(
            WORKFLOW_START_REQUIRED_CAPABILITIES,
            [
                types::Capability::SessionRead,
                types::Capability::SessionWrite,
                types::Capability::PageWrite,
            ]
        );
        assert_eq!(
            WORKFLOW_START_OPERATION,
            types::InterfaceOperation::CreateSession
        );
        assert_eq!(
            WORKFLOW_OBSERVE_REQUIRED_CAPABILITIES,
            [types::Capability::BrowserMutate]
        );
        assert_eq!(
            WORKFLOW_OBSERVE_OPERATION,
            types::InterfaceOperation::SubmitCommand
        );
    }

    #[test]
    fn workflow_observe_goal_uses_unicode_scalar_advertising_semantics() {
        let advertised = advertised_tool_schema("workflow_observe");
        let validator = jsonschema::validator_for(&advertised).expect("valid advertised schema");
        let accepted = json!({
            "workflowHandle":"wf_0123456789abcdef0123456789abcdef",
            "goal":"é".repeat(MAX_WORKFLOW_GOAL_SCALARS)
        });
        let rejected = json!({
            "workflowHandle":"wf_0123456789abcdef0123456789abcdef",
            "goal":"é".repeat(MAX_WORKFLOW_GOAL_SCALARS + 1)
        });
        assert!(validator.is_valid(&accepted));
        assert!(!validator.is_valid(&rejected));
        assert!(validate_tool_arguments("workflow_observe", &accepted).is_ok());
    }

    #[test]
    fn workflow_scope_allowlist_is_sorted_and_exactly_matches_advertised_handle_schemas() {
        let expected = WORKFLOW_SCOPE_TOOLS
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>();
        assert!(expected.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(
            expected.len(),
            expected.iter().collect::<BTreeSet<_>>().len()
        );

        let observed = crate::toolset::EVERY_TOOL
            .iter()
            .filter(|name| {
                advertised_tool_schema(name)["oneOf"]
                    .as_array()
                    .is_some_and(|branches| {
                        branches.iter().any(|branch| {
                            branch["required"] == json!(["workflowHandle"])
                                && branch["properties"]["sessionId"] == Value::Bool(false)
                                && branch["properties"]["pageId"] == Value::Bool(false)
                        })
                    })
            })
            .copied()
            .collect::<BTreeSet<_>>();
        assert_eq!(
            observed.into_iter().collect::<Vec<_>>(),
            expected,
            "workflowHandle advertising must exactly match the normalization table"
        );
    }

    #[test]
    fn workflow_scope_branches_repeat_business_properties_and_keep_parent_requirements() {
        for (name, business_property) in [
            ("navigate", "url"),
            ("intent_fill", "value"),
            ("wait_for", "condition"),
        ] {
            let schema = advertised_tool_schema(name);
            let expected = schema["properties"][business_property].clone();
            assert!(
                schema["required"]
                    .as_array()
                    .expect("required array")
                    .contains(&json!(business_property)),
                "{name} keeps {business_property} required at the parent"
            );
            for branch in schema["oneOf"].as_array().expect("scope branches") {
                assert_eq!(
                    branch["properties"][business_property], expected,
                    "{name} branch carries {business_property}'s full schema"
                );
            }
        }
    }

    #[test]
    fn session_creation_policy_visibility_is_shared_by_deferred_workflow_start() {
        let unprivileged = types::CapabilitySet::new([]);
        let privileged = types::CapabilitySet::new([
            types::Capability::BrowserFingerprint,
            types::Capability::BrowserHumanize,
        ]);
        for name in ["session_create", "workflow_start"] {
            let hidden = advertised_tool_schema_for_capabilities(name, &unprivileged);
            let hidden = &hidden["properties"]["executionPolicy"]["properties"];
            assert!(hidden.get("fingerprint").is_none(), "{name}");
            assert!(hidden.get("humanize").is_none(), "{name}");

            let shown = advertised_tool_schema_for_capabilities(name, &privileged);
            let shown = &shown["properties"]["executionPolicy"]["properties"];
            assert!(shown.get("fingerprint").is_some(), "{name}");
            assert!(shown.get("humanize").is_some(), "{name}");
        }
    }

    #[test]
    fn zigzagzig_visibility_requires_both_browser_capabilities() {
        let fingerprint_only = types::CapabilitySet::new([types::Capability::BrowserFingerprint]);
        let humanize_only = types::CapabilitySet::new([types::Capability::BrowserHumanize]);
        let both = types::CapabilitySet::new([
            types::Capability::BrowserFingerprint,
            types::Capability::BrowserHumanize,
        ]);
        for name in ["session_create", "workflow_start"] {
            for capabilities in [&fingerprint_only, &humanize_only] {
                let schema = advertised_tool_schema_for_capabilities(name, capabilities);
                assert!(
                    schema["properties"].get("zigzagzig").is_none(),
                    "{name} hides godmode from a principal missing one capability"
                );
            }
            let schema = advertised_tool_schema_for_capabilities(name, &both);
            assert!(
                schema["properties"].get("zigzagzig").is_some(),
                "{name} shows godmode to a principal holding both capabilities"
            );
        }
    }
}

/// Must match `types::FormControlState`'s `tag = "kind"` serde output.
fn form_control_state_variants() -> Vec<Value> {
    vec![
        tagged_fields("empty", json!({}), &[]),
        tagged_fields(
            "text",
            json!({"value":string(0, MAX_STRING_BYTES)}),
            &["value"],
        ),
        tagged_fields(
            "redacted",
            json!({"present":{"type":"boolean"}}),
            &["present"],
        ),
        tagged_fields(
            "checked",
            json!({"checked":{"type":"boolean"}}),
            &["checked"],
        ),
        tagged_fields(
            "selection",
            json!({"values":array(string(0, MAX_STRING_BYTES), 512)}),
            &["values"],
        ),
        tagged_fields(
            "files",
            json!({"count":{"type":"integer","minimum":0}}),
            &["count"],
        ),
    ]
}

/// Must match `types::FormControlValidity`.
fn form_control_validity() -> Value {
    object(
        json!({
            "willValidate":{"type":"boolean"},
            "valid":{"type":"boolean"},
            "flags":array(json!({"type":"string","enum":[
                "valueMissing","typeMismatch","patternMismatch","tooLong","tooShort",
                "rangeUnderflow","rangeOverflow","stepMismatch","badInput","customError"
            ]}), 10),
            "message":nullable(string(0, 1024)),
            "describedBy":array(string(0, MAX_STRING_BYTES), 512)
        }),
        &["willValidate", "valid", "flags", "message", "describedBy"],
    )
}

/// Must match `types::FormOption`.
fn form_option() -> Value {
    object(
        json!({
            "value":string(0, MAX_STRING_BYTES),
            "label":string(0, MAX_STRING_BYTES),
            "disabled":{"type":"boolean"},
            "selected":{"type":"boolean"},
            "groupLabel":nullable(string(0, MAX_STRING_BYTES))
        }),
        &["value", "label", "disabled", "selected", "groupLabel"],
    )
}

fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}

fn tagged_input(kind: &str, input: Value) -> Value {
    object(
        json!({"kind":{"const":kind},"input":input}),
        &["kind", "input"],
    )
}

fn tagged_content(kind: &str, value: Value) -> Value {
    object(
        json!({"kind":{"const":kind},"value":value}),
        &["kind", "value"],
    )
}

fn tagged_fields(kind: &str, fields: Value, required: &[&str]) -> Value {
    discriminated_object("kind", kind, fields, required)
}

fn status_fields(status: &str, fields: Value, required: &[&str]) -> Value {
    discriminated_object("status", status, fields, required)
}

fn discriminated_object(tag: &str, discriminant: &str, fields: Value, required: &[&str]) -> Value {
    let mut properties = fields.as_object().cloned().unwrap_or_default();
    properties.insert(tag.to_owned(), json!({"const":discriminant}));
    let mut all_required = required.to_vec();
    all_required.push(tag);
    object(Value::Object(properties), &all_required)
}

fn string(min: usize, max: usize) -> Value {
    // minLength:0 is the JSON Schema default — emitting it is pure bytes.
    if min == 0 {
        json!({"type":"string","maxLength":max})
    } else {
        json!({"type":"string","minLength":min,"maxLength":max})
    }
}

fn timeout_ms() -> Value {
    json!({"type":"integer","minimum":1,"maximum":MAX_TIMEOUT_MS})
}
fn id() -> Value {
    json!({"$ref":"#/$defs/Id"})
}

/// Caller-pinned ids (commandId/attemptId on Boundary-capable tools). The
/// length bounds `id()` carries are dropped here to keep `tools/list` inside
/// the connect byte budget; the types themselves reject a non-UUID on parse.
fn id_pin() -> Value {
    json!({"type":"string","format":"uuid"})
}
fn sha256() -> Value {
    json!({"type":"string","minLength":64,"maxLength":64,"pattern":"^[0-9a-f]{64}$"})
}
fn array(items: Value, max: usize) -> Value {
    json!({"type":"array","items":items,"maxItems":max})
}
fn nonempty_array(items: Value, max: usize) -> Value {
    json!({"type":"array","items":items,"minItems":1,"maxItems":max})
}
fn nullable(schema: Value) -> Value {
    json!({"oneOf":[schema,{"type":"null"}]})
}
/// An unconstrained schema (no `type`, `oneOf`, `const`, or `enum`); `validate` accepts
/// any JSON value against it. Used for `Evidence::JavaScriptResult.value`, which carries
/// an arbitrary `serde_json::Value` produced by evaluated JavaScript.
fn any_value() -> Value {
    json!({})
}
fn finite_number() -> Value {
    json!({"type":"number","minimum":-1000000000.0,"maximum":1000000000.0})
}
fn positive_number() -> Value {
    json!({"type":"number","exclusiveMinimum":0.0,"maximum":1000000000.0})
}

/// Why a tool argument was rejected, and where. `pointer` is a JSON Pointer into the
/// arguments; `constraint` names the keyword that failed. Neither ever carries the
/// offending value, so this leaks no more than the published schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SchemaViolation {
    pub(crate) pointer: String,
    pub(crate) constraint: &'static str,
}

impl SchemaViolation {
    fn at(pointer: &str, constraint: &'static str) -> Self {
        Self {
            pointer: if pointer.is_empty() {
                "/".to_owned()
            } else {
                pointer.to_owned()
            },
            constraint,
        }
    }
}

type Validated = Result<(), SchemaViolation>;

/// Escapes a JSON Pointer token per RFC 6901.
fn push_pointer(pointer: &str, token: &str) -> String {
    format!("{pointer}/{}", token.replace('~', "~0").replace('/', "~1"))
}

fn validate(root: &Value, schema: &Value, value: &Value) -> Validated {
    validate_at(root, schema, value, 0, "")
}

/// Bounds validator recursion independently of schema shape. `AccessibilityNode` is
/// self-referential, so without this guard a deeply nested argument within the 256 KiB
/// input cap would exhaust the stack. Sized above the deepest legitimate argument (a
/// `checkpoint_save` accessibility tree, two levels per node).
const MAX_VALIDATION_DEPTH: usize = 128;

fn validate_at(
    root: &Value,
    schema: &Value,
    value: &Value,
    depth: usize,
    pointer: &str,
) -> Validated {
    if depth > MAX_VALIDATION_DEPTH {
        return Err(SchemaViolation::at(pointer, "maxDepth"));
    }
    let depth = depth + 1;
    // Agent-facing argument templates. Not enforced — this `.get` is also how
    // `budget.rs` discovers supported keywords for advertised `inputSchema`s.
    let _ = schema.get("examples");
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let Some(name) = reference.strip_prefix("#/$defs/") else {
            return Err(SchemaViolation::at(pointer, "$ref"));
        };
        let Some(target) = root.get("$defs").and_then(|defs| defs.get(name)) else {
            return Err(SchemaViolation::at(pointer, "$ref"));
        };
        return validate_at(root, target, value, depth, pointer);
    }
    if let Some(choices) = schema.get("oneOf").and_then(Value::as_array) {
        // Report the branch point rather than one arbitrary branch's failure:
        // with a discriminated union the useful signal is "this value matched
        // no variant", not why the last variant disagreed.
        let matches = choices
            .iter()
            .filter(|choice| validate_at(root, choice, value, depth, pointer).is_ok())
            .count();
        return if matches == 1 {
            Ok(())
        } else {
            Err(SchemaViolation::at(pointer, "oneOf"))
        };
    }
    if let Some(expected) = schema.get("const") {
        return if value == expected {
            Ok(())
        } else {
            Err(SchemaViolation::at(pointer, "const"))
        };
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        if !values.contains(value) {
            return Err(SchemaViolation::at(pointer, "enum"));
        }
    }
    if let Some(value_type) = schema.get("type") {
        let matches = match value_type {
            Value::String(kind) => matches_type(kind, value),
            Value::Array(kinds) => kinds
                .iter()
                .filter_map(Value::as_str)
                .any(|kind| matches_type(kind, value)),
            _ => false,
        };
        if !matches {
            return Err(SchemaViolation::at(pointer, "type"));
        }
        if value.is_null() {
            return Ok(());
        }
    }
    match value {
        Value::Object(values) => validate_object(root, schema, values, depth, pointer),
        Value::Array(values) => {
            if schema
                .get("maxItems")
                .and_then(Value::as_u64)
                .is_some_and(|max| values.len() as u64 > max)
            {
                return Err(SchemaViolation::at(pointer, "maxItems"));
            }
            if schema
                .get("minItems")
                .and_then(Value::as_u64)
                .is_some_and(|min| (values.len() as u64) < min)
            {
                return Err(SchemaViolation::at(pointer, "minItems"));
            }
            if schema
                .get("uniqueItems")
                .and_then(Value::as_bool)
                .is_some_and(|required| required)
                && values
                    .iter()
                    .enumerate()
                    .any(|(index, value)| values[..index].contains(value))
            {
                return Err(SchemaViolation::at(pointer, "uniqueItems"));
            }
            let Some(items) = schema.get("items") else {
                return Ok(());
            };
            for (index, value) in values.iter().enumerate() {
                validate_at(
                    root,
                    items,
                    value,
                    depth,
                    &push_pointer(pointer, &index.to_string()),
                )?;
            }
            Ok(())
        }
        Value::String(value) => validate_string(schema, value, pointer),
        Value::Number(number) => validate_number(schema, number.as_f64(), pointer),
        _ => Ok(()),
    }
}

fn matches_type(kind: &str, value: &Value) -> bool {
    match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

fn validate_object(
    root: &Value,
    schema: &Value,
    values: &Map<String, Value>,
    depth: usize,
    pointer: &str,
) -> Validated {
    if schema
        .get("maxProperties")
        .and_then(Value::as_u64)
        .is_some_and(|max| values.len() as u64 > max)
    {
        return Err(SchemaViolation::at(pointer, "maxProperties"));
    }
    let properties = schema.get("properties").and_then(Value::as_object);
    if let Some(property_names) = schema.get("propertyNames") {
        for key in values.keys() {
            let name = Value::String(key.clone());
            if validate_at(root, property_names, &name, depth, pointer).is_err() {
                return Err(SchemaViolation::at(
                    &push_pointer(pointer, key),
                    "propertyNames",
                ));
            }
        }
    }
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for key in required.iter().filter_map(Value::as_str) {
            if !values.contains_key(key) {
                return Err(SchemaViolation::at(&push_pointer(pointer, key), "required"));
            }
        }
    }
    for (key, value) in values {
        let child = push_pointer(pointer, key);
        if let Some(property) = properties.and_then(|properties| properties.get(key)) {
            validate_at(root, property, value, depth, &child)?;
        } else {
            match schema.get("additionalProperties") {
                Some(Value::Bool(false)) => {
                    return Err(SchemaViolation::at(&child, "additionalProperties"))
                }
                Some(additional) if additional.is_object() => {
                    validate_at(root, additional, value, depth, &child)?;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn validate_string(schema: &Value, value: &str, pointer: &str) -> Validated {
    if schema
        .get("minLength")
        .and_then(Value::as_u64)
        .is_some_and(|min| (value.len() as u64) < min)
    {
        return Err(SchemaViolation::at(pointer, "minLength"));
    }
    if schema
        .get("maxLength")
        .and_then(Value::as_u64)
        .is_some_and(|max| value.len() as u64 > max)
    {
        return Err(SchemaViolation::at(pointer, "maxLength"));
    }
    if schema.get("format") == Some(&json!("uuid")) && uuid::Uuid::parse_str(value).is_err() {
        return Err(SchemaViolation::at(pointer, "format"));
    }
    if schema.get("format") == Some(&json!("date-time"))
        && chrono::DateTime::parse_from_rfc3339(value).is_err()
    {
        return Err(SchemaViolation::at(pointer, "format"));
    }
    // `pattern` does NOT evaluate the declared regex: this crate has no regex engine, so
    // the character class of the one declared pattern, `sha256()`'s `^[0-9a-f]{64}$`, is
    // hardcoded here. A second, different `pattern` would silently get this check;
    // `budget.rs::the_only_declared_pattern_is_the_one_the_validator_implements` fails
    // the build if one is added.
    if schema.get("pattern").is_some()
        && !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(SchemaViolation::at(pointer, "pattern"));
    }
    Ok(())
}

fn validate_number(schema: &Value, number: Option<f64>, pointer: &str) -> Validated {
    let Some(number) = number.filter(|number| number.is_finite()) else {
        return Err(SchemaViolation::at(pointer, "type"));
    };
    if schema
        .get("minimum")
        .and_then(Value::as_f64)
        .is_some_and(|min| number < min)
    {
        return Err(SchemaViolation::at(pointer, "minimum"));
    }
    if schema
        .get("exclusiveMinimum")
        .and_then(Value::as_f64)
        .is_some_and(|min| number <= min)
    {
        return Err(SchemaViolation::at(pointer, "exclusiveMinimum"));
    }
    if schema
        .get("maximum")
        .and_then(Value::as_f64)
        .is_some_and(|max| number > max)
    {
        return Err(SchemaViolation::at(pointer, "maximum"));
    }
    Ok(())
}

/// Self-referential rather than depth-inlined; depth-inlining costs ~40 KiB per tool
/// carrying `Evidence`. Recursion is bounded by `MAX_VALIDATION_DEPTH`, not by shape.
fn accessibility_node() -> Value {
    let mut schema = object(
        json!({
            "role":string(1, 256),
            "name":string(1, 4096),
            "target":{"$ref":"#/$defs/AccessibilityTarget"},
            "value":string(0, 4096),
            "description":string(0, 4096),
            "required":{"type":"boolean"},
            "disabled":{"type":"boolean"},
            "readOnly":{"type":"boolean"},
            "invalid":{"type":"boolean"},
            "checked":{"type":"boolean"},
            "autocomplete":string(0, 256),
            "url":string(1, 4096),
            "valueMin":string(0, 256),
            "valueMax":string(0, 256)
        }),
        &[],
    );
    schema["properties"]["children"] = array(json!({"$ref":"#/$defs/AccessibilityNode"}), 256);
    schema
}

/// Must match `types::AccessibilityTarget`. `framePath` (an array of
/// `SemanticTargetSegment`) stays generic rather than a `$ref`, mirroring
/// `form_control_target`: rarely populated, and present only on nodes that
/// live inside an iframe.
fn accessibility_target() -> Value {
    object(
        json!({
            "role":string(1, 256),
            "accessibleName":string(1, 4096),
            "ordinal":{"type":"integer","minimum":0,"maximum":2047},
            "framePath":array(json!({"type":"object"}), 8)
        }),
        &["role", "accessibleName"],
    )
}
