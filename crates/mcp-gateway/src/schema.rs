use std::collections::BTreeSet;

use serde_json::{json, Map, Value};

use crate::protocol::MAX_EVENT_LIMIT;

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
// Output-only: bounds for definitions reachable solely from `structuredContent`,
// never from a tool argument.
const MAX_RECOVERY_RECEIPTS: usize = 64;

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
                )
            }),
            vec!["profile"],
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
                "maxNodes": {"type":"integer","minimum":1,"maximum":2048}
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
                    "required":["role","accessibleName","ordinal","framePath","shadowPath"]
                },
                "action": {"oneOf":[
                    {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"setText"},"value":string(0,4096)},"required":["kind","value"]},
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
                "sessionId": id(),
                "pageId": id(),
                "clear": {"type":"boolean"}
            }),
            vec!["sessionId", "pageId"],
        ),
        "emulate" => (
            json!({
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
                "sessionId": id(),
                "pageId": id(),
                "action": {"type":"string","enum":["accept","dismiss"]},
                "timeoutMs": timeout_ms()
            }),
            vec!["sessionId", "pageId", "action"],
        ),
        "pdf" => (
            json!({
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
                "sessionId": id(),
                "pageId": id(),
                "urls": array(string(1, MAX_URL_BYTES), 64)
            }),
            vec!["sessionId", "pageId"],
        ),
        "cookie_set" => (
            json!({
                "sessionId": id(),
                "pageId": id(),
                "cookies": array(json!({"$ref":"#/$defs/SetCookieParam"}), 128)
            }),
            vec!["sessionId", "pageId", "cookies"],
        ),
        "cookie_delete" => (
            json!({
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
                "expectedUrl": nullable(string(1, MAX_URL_BYTES))
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
                "maxBytes": {"type":"integer","minimum":1,"maximum":1099511627776_u64}
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
                "fields":nonempty_array(json!({"$ref":"#/$defs/CompleteFormField"}), MAX_COLLECTION_ITEMS)
            })),
            intent_required(&["purpose", "fields"]),
        ),
        "intent_submit_and_verify" => (
            intent_properties(json!({"expectedState":{"$ref":"#/$defs/WaitForCommand"}})),
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
                "boundary":{"type":"boolean"}
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
        "recovery_status" => (json!({"workflowId":id()}), vec!["workflowId"]),
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
/// Identical to `tool_schema` for every tool but `command_execute`, whose
/// `envelope.command` advertises as an opaque object instead of the 13,783-byte
/// `PrimitiveCommand`/`IntentCommand` union. Only advertisement narrows: `tool_schema`
/// and `definitions()` keep the full union, so a malformed nested command still fails
/// `-32602` before reaching the runtime. The four primitives with no named tool
/// (`clickAndWaitForPopup`, `clickAndWaitForDownload`, `setFocusEmulation`,
/// `setEmulatedMedia`) are documented in the `bobby://primitives` resource.
pub(crate) fn advertised_tool_schema(name: &str) -> Value {
    let mut schema = if name != "command_execute" {
        tool_schema(name)
    } else {
        let mut schema = tool_schema(name);
        let mut patched = definitions();
        let patched = patched.as_object_mut().expect("definitions is an object");
        if let Some(command_envelope) = patched.get_mut("CommandEnvelope") {
            command_envelope["properties"]["command"] = json!({
                "type": "object",
                "description": "One command as {\"kind\":\"primitive\"|\"intent\",\"input\":{…}}. \
            Prefer named tools for common actions; they build this envelope."
            });
        }
        let seed = json!({"properties": schema["properties"], "required": schema["required"]});
        schema["$defs"] = reachable_definitions_from(&seed, patched);
        schema
    };
    if let Some(example) = tool_argument_example(name) {
        schema["examples"] = json!([example]);
    }
    schema
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
        "recovery_status" => object(
            json!({
                "workflowId":id(),
                "checkpoint":{"$ref":"#/$defs/CheckpointRecord"},
                // `RecoveryReceipt` also carries a `CommandOutcome`, a
                // `SkillOutcome`, and a `SkillDecision`; kept generic here
                // for the same reason as `CheckpointRecord.recoveryReceipts`.
                "receipts":array(json!({"type":"object"}), MAX_RECOVERY_RECEIPTS)
            }),
            &["workflowId", "checkpoint", "receipts"],
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

fn fill_values() -> Vec<Value> {
    vec![
        tagged_fields(
            "text",
            json!({
                "text":string(0, MAX_STRING_BYTES),
                "clearFirst":{"type":"boolean"}
            }),
            &["text"],
        ),
        tagged_fields(
            "select",
            json!({"option":string(0, MAX_STRING_BYTES)}),
            &["option"],
        ),
        tagged_fields(
            "checked",
            json!({"checked":{"type":"boolean"}}),
            &["checked"],
        ),
        tagged_fields(
            "files",
            json!({"paths":array(string(1, MAX_STRING_BYTES), 64)}),
            &["paths"],
        ),
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
                        {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"setText"},"value":string(0,4096)},"required":["kind","value"]},
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
                "reason":{"type":"string","enum":["eligibleStaticDocument","eligibleExplicitDownload","ineligibleCommand","semanticTargetRequired","javascriptRequired","unsupportedContentType","stateConflict","policyRequired"]},
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
            json!({"filename":string(1,MAX_STRING_BYTES),"path":string(1,MAX_STRING_BYTES),"bytes":{"type":"integer","minimum":0},"sha256":sha256()}),
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
        "visionAssistDenied","visionAssistFailed","targetObscured","targetOutOfBounds"
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
            )
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

/// Must match `types::FormControlTarget`. `framePath`/`shadowPath` (arrays of
/// `SemanticTargetSegment`) stay generic rather than a `$ref`: rarely populated, and the
/// same three scalar fields as `role`/`accessibleName`/`ordinal` here.
fn form_control_target() -> Value {
    object(
        json!({
            "role":string(0, 128),
            "accessibleName":string(0, MAX_STRING_BYTES),
            "ordinal":nullable(json!({"type":"integer","minimum":0})),
            "framePath":array(json!({"type":"object"}), 8),
            "shadowPath":array(json!({"type":"object"}), 8)
        }),
        &[
            "role",
            "accessibleName",
            "ordinal",
            "framePath",
            "shadowPath",
        ],
    )
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
    json!({"type":"string","minLength":min,"maxLength":max})
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
            "valueMin":string(0, 256),
            "valueMax":string(0, 256)
        }),
        &[],
    );
    schema["properties"]["children"] = array(json!({"$ref":"#/$defs/AccessibilityNode"}), 256);
    schema
}

fn accessibility_target() -> Value {
    object(
        json!({
            "role":string(1, 256),
            "accessibleName":string(1, 4096),
            "ordinal":{"type":"integer","minimum":0,"maximum":2047}
        }),
        &["role", "accessibleName"],
    )
}
