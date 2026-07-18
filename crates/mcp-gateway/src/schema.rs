use serde_json::{json, Map, Value};

use crate::protocol::MAX_EVENT_LIMIT;

pub(crate) const MAX_EVIDENCE_ITEMS: usize = 128;
const MAX_COLLECTION_ITEMS: usize = 128;
const MAX_STRING_BYTES: usize = 4096;
const MAX_URL_BYTES: usize = 8192;
const MAX_HTML_BYTES: usize = 64 * 1024;
const MAX_TIMEOUT_MS: u64 = 300_000;

pub(crate) fn validate_tool_arguments(name: &str, arguments: &Value) -> bool {
    let schema = tool_schema(name);
    validate(&schema, &schema, arguments)
}

pub(crate) fn tool_schema(name: &str) -> Value {
    let (properties, required) = match name {
        "runtime_info" | "session_list" => (json!({}), vec![]),
        "session_create" => (
            json!({
                "profile": string(1, 128),
                "proxy": nullable(string(0, 2048))
            }),
            vec!["profile"],
        ),
        "page_open" => (json!({"sessionId": id()}), vec!["sessionId"]),
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
                "evidence":array(json!({"$ref":"#/$defs/Evidence"}), MAX_EVIDENCE_ITEMS)
            }),
            vec!["checkpoint"],
        ),
        "workflow_recover" => (json!({"workflowId":id()}), vec!["workflowId"]),
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
    schema["$defs"] = definitions();
    schema
}

fn definitions() -> Value {
    json!({
        "CommandEnvelope": object(json!({
            "schemaVersion":{"type":"integer","const":1},
            "commandId":id(), "workflowId":id(), "attemptId":id(), "sessionId":id(),
            "pageId":nullable(id()),
            "deadline":{"type":"string","format":"date-time","minLength":20,"maxLength":64},
            "command":{"$ref":"#/$defs/PrimitiveCommand"}
        }), &["schemaVersion","commandId","workflowId","attemptId","sessionId","deadline","command"]),
        "PrimitiveCommand": {"oneOf": primitive_commands()},
        "TargetSpec": target_spec(),
        "TextMatch": {"oneOf":[
            tagged_content("exact", string(0, MAX_STRING_BYTES)),
            tagged_content("contains", string(0, MAX_STRING_BYTES)),
            tagged_content("regex", string(0, MAX_STRING_BYTES))
        ]},
        "WaitCondition": {"oneOf": wait_conditions()},
        "ScreenshotMode": {"oneOf": screenshot_modes()},
        "Evidence": {"oneOf": evidence_variants()},
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
        "WorkflowCheckpoint": object(json!({
            "schemaVersion":{"type":"integer","const":1},
            "checkpointId":id(), "workflowId":id(), "attemptId":id(), "sessionId":id(), "pageId":id(),
            "restartUrl":string(1, MAX_URL_BYTES), "currentUrl":string(1, MAX_URL_BYTES),
            "cursor":nullable(id()), "boundaryCommandId":nullable(id()),
            "recoveryClass":{"type":"string","enum":["replayable","reconciliable","boundary"]},
            "invariants":array(json!({"$ref":"#/$defs/CheckpointInvariant"}), MAX_COLLECTION_ITEMS),
            "replayableInputs":array(string(0, MAX_STRING_BYTES), MAX_COLLECTION_ITEMS),
            "evidence":array(json!({"$ref":"#/$defs/Evidence"}), MAX_EVIDENCE_ITEMS),
            "recoveryHistory":array(json!({"$ref":"#/$defs/RecoveryRecord"}), MAX_COLLECTION_ITEMS),
            "createdAt":{"type":"string","format":"date-time","minLength":20,"maxLength":64}
        }), &["schemaVersion","checkpointId","workflowId","attemptId","sessionId","pageId","restartUrl","currentUrl","recoveryClass","invariants","replayableInputs","evidence","createdAt"])
    })
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
            "openPage",
            object(json!({"url":nullable(string(0, MAX_URL_BYTES))}), &["url"]),
        ),
        tagged_input("listPages", json!({"type":"null"})),
        tagged_input("closePage", object(json!({"pageId":id()}), &["pageId"])),
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
            json!({"idle_ms":{"type":"integer","minimum":0,"maximum":MAX_TIMEOUT_MS},"max_in_flight":{"type":"integer","minimum":0,"maximum":10000}}),
            &["idle_ms", "max_in_flight"],
        ),
    ]
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
                "state_version":{"type":"integer","minimum":0},"elapsed_ms":{"type":"integer","minimum":0},
                "bytes":nullable(json!({"type":"integer","minimum":0})),"sha256":nullable(sha256()),
                "final_url":nullable(string(0, MAX_URL_BYTES)),"content_type":nullable(string(0,256)),
                "status":nullable(json!({"type":"integer","minimum":100,"maximum":599})),
                "redirect_chain":array(string(0, MAX_URL_BYTES), 32)
            }),
            &[
                "path",
                "reason",
                "state_version",
                "elapsed_ms",
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
            &["page_id", "url", "title"],
        ),
        tagged_fields(
            "pages",
            json!({"pages":array(object(page_evidence_properties(), &["page_id","url","title"]),MAX_COLLECTION_ITEMS)}),
            &["pages"],
        ),
        tagged_fields(
            "popup",
            json!({"opener_page_id":id(),"page_id":id(),"url":string(0,MAX_URL_BYTES),"title":string(0,MAX_STRING_BYTES)}),
            &["opener_page_id", "page_id", "url", "title"],
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
                "best_match_authorized":{"type":"boolean"}
            }),
            &[
                "target",
                "fingerprint",
                "candidates",
                "best_match_authorized",
            ],
        ),
        tagged_fields(
            "wait",
            json!({"condition":{"$ref":"#/$defs/WaitCondition"},"elapsed_ms":{"type":"integer","minimum":0},"observations":{"type":"integer","minimum":0}}),
            &["condition", "elapsed_ms", "observations"],
        ),
        tagged_fields(
            "screenshot",
            json!({"artifact_id":string(1,128),"media_type":string(1,256),"width":{"type":"integer","minimum":1,"maximum":16384},"height":{"type":"integer","minimum":1,"maximum":16384},"bytes":{"type":"integer","minimum":1,"maximum":1073741824u64},"sha256":sha256()}),
            &[
                "artifact_id",
                "media_type",
                "width",
                "height",
                "bytes",
                "sha256",
            ],
        ),
    ]
}

fn page_evidence_properties() -> Value {
    json!({"page_id":id(),"url":string(0,MAX_URL_BYTES),"title":string(0,MAX_STRING_BYTES)})
}

fn recovery_decisions() -> Vec<Value> {
    vec![
        status_fields(
            "resumed",
            json!({"checkpoint_id":id(),"attempt_id":id(),"evidence":array(json!({"$ref":"#/$defs/Evidence"}),MAX_EVIDENCE_ITEMS)}),
            &["checkpoint_id", "attempt_id", "evidence"],
        ),
        status_fields(
            "needsReconciliation",
            json!({"checkpoint_id":id(),"attempt_id":id(),"reason":string(1,MAX_STRING_BYTES),"evidence":array(json!({"$ref":"#/$defs/Evidence"}),MAX_EVIDENCE_ITEMS)}),
            &["checkpoint_id", "attempt_id", "reason", "evidence"],
        ),
        status_fields(
            "restarted",
            json!({"checkpoint_id":id(),"lineage":object(json!({"workflowId":id(),"abandonedAttemptId":id(),"attemptId":id(),"reason":string(1,MAX_STRING_BYTES)}), &["workflowId","abandonedAttemptId","attemptId","reason"])}),
            &["checkpoint_id", "lineage"],
        ),
    ]
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
fn id() -> Value {
    json!({"type":"string","format":"uuid","minLength":36,"maxLength":36})
}
fn sha256() -> Value {
    json!({"type":"string","minLength":64,"maxLength":64,"pattern":"^[0-9a-f]{64}$"})
}
fn array(items: Value, max: usize) -> Value {
    json!({"type":"array","items":items,"maxItems":max})
}
fn nullable(schema: Value) -> Value {
    json!({"oneOf":[schema,{"type":"null"}]})
}
fn finite_number() -> Value {
    json!({"type":"number","minimum":-1000000000.0,"maximum":1000000000.0})
}
fn positive_number() -> Value {
    json!({"type":"number","exclusiveMinimum":0.0,"maximum":1000000000.0})
}

fn validate(root: &Value, schema: &Value, value: &Value) -> bool {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let Some(name) = reference.strip_prefix("#/$defs/") else {
            return false;
        };
        let Some(target) = root.get("$defs").and_then(|defs| defs.get(name)) else {
            return false;
        };
        return validate(root, target, value);
    }
    if let Some(choices) = schema.get("oneOf").and_then(Value::as_array) {
        return choices
            .iter()
            .filter(|choice| validate(root, choice, value))
            .count()
            == 1;
    }
    if let Some(expected) = schema.get("const") {
        return value == expected;
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        if !values.contains(value) {
            return false;
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
            return false;
        }
        if value.is_null() {
            return true;
        }
    }
    match value {
        Value::Object(values) => validate_object(root, schema, values),
        Value::Array(values) => {
            if schema
                .get("maxItems")
                .and_then(Value::as_u64)
                .is_some_and(|max| values.len() as u64 > max)
            {
                return false;
            }
            if schema
                .get("minItems")
                .and_then(Value::as_u64)
                .is_some_and(|min| (values.len() as u64) < min)
            {
                return false;
            }
            schema
                .get("items")
                .is_none_or(|items| values.iter().all(|value| validate(root, items, value)))
        }
        Value::String(value) => validate_string(schema, value),
        Value::Number(number) => validate_number(schema, number.as_f64()),
        _ => true,
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

fn validate_object(root: &Value, schema: &Value, values: &Map<String, Value>) -> bool {
    if schema
        .get("maxProperties")
        .and_then(Value::as_u64)
        .is_some_and(|max| values.len() as u64 > max)
    {
        return false;
    }
    let properties = schema.get("properties").and_then(Value::as_object);
    if schema.get("propertyNames").is_some_and(|property_names| {
        values
            .keys()
            .any(|key| !validate(root, property_names, &Value::String(key.clone())))
    }) {
        return false;
    }
    if schema
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|required| {
            required
                .iter()
                .filter_map(Value::as_str)
                .any(|key| !values.contains_key(key))
        })
    {
        return false;
    }
    for (key, value) in values {
        if let Some(property) = properties.and_then(|properties| properties.get(key)) {
            if !validate(root, property, value) {
                return false;
            }
        } else {
            match schema.get("additionalProperties") {
                Some(Value::Bool(false)) => return false,
                Some(additional)
                    if additional.is_object() && !validate(root, additional, value) =>
                {
                    return false
                }
                _ => {}
            }
        }
    }
    true
}

fn validate_string(schema: &Value, value: &str) -> bool {
    if schema
        .get("minLength")
        .and_then(Value::as_u64)
        .is_some_and(|min| (value.len() as u64) < min)
    {
        return false;
    }
    if schema
        .get("maxLength")
        .and_then(Value::as_u64)
        .is_some_and(|max| value.len() as u64 > max)
    {
        return false;
    }
    if schema.get("format") == Some(&json!("uuid")) && uuid::Uuid::parse_str(value).is_err() {
        return false;
    }
    if schema.get("format") == Some(&json!("date-time"))
        && chrono::DateTime::parse_from_rfc3339(value).is_err()
    {
        return false;
    }
    if schema.get("pattern").is_some()
        && !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return false;
    }
    true
}

fn validate_number(schema: &Value, number: Option<f64>) -> bool {
    let Some(number) = number.filter(|number| number.is_finite()) else {
        return false;
    };
    if schema
        .get("minimum")
        .and_then(Value::as_f64)
        .is_some_and(|min| number < min)
    {
        return false;
    }
    if schema
        .get("exclusiveMinimum")
        .and_then(Value::as_f64)
        .is_some_and(|min| number <= min)
    {
        return false;
    }
    if schema
        .get("maximum")
        .and_then(Value::as_f64)
        .is_some_and(|max| number > max)
    {
        return false;
    }
    true
}
