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
                "proxy": nullable(string(0, 2048)),
                "executionPolicy": object(
                    json!({
                        "javascriptEvaluation":{"type":"boolean"},
                        "visionAssist":{"type":"boolean"}
                    }),
                    &[]
                )
            }),
            vec!["profile"],
        ),
        "page_open" => (json!({"sessionId": id()}), vec!["sessionId"]),
        "page_close" => (
            json!({"sessionId": id(), "pageId": id()}),
            vec!["sessionId", "pageId"],
        ),
        "page_activate" => (
            json!({"sessionId": id(), "pageId": id()}),
            vec!["sessionId", "pageId"],
        ),
        "a11y_snapshot" => (
            json!({
                "sessionId": id(),
                "pageId": id(),
                "maxNodes": {"type":"integer","minimum":1,"maximum":2048}
            }),
            vec!["sessionId", "pageId"],
        ),
        "session_close" => (json!({"sessionId": id()}), vec!["sessionId"]),
        "navigate" => (
            json!({
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
                "sessionId": id(),
                "pageId": id(),
                "mode": {"$ref":"#/$defs/ScreenshotMode"}
            }),
            vec!["sessionId", "pageId"],
        ),
        "wait_for" => (
            json!({
                "sessionId": id(),
                "pageId": id(),
                "condition": {"$ref":"#/$defs/WaitCondition"},
                "timeoutMs": timeout_ms()
            }),
            vec!["sessionId", "pageId", "condition", "timeoutMs"],
        ),
        "page_list" => (json!({"sessionId": id()}), vec!["sessionId"]),
        "download_url" => (
            json!({
                "sessionId": id(),
                "url": string(1, MAX_URL_BYTES),
                "expectedContentType": nullable(string(1, 256)),
                "maxBytes": {"type":"integer","minimum":1,"maximum":1099511627776_u64}
            }),
            vec!["sessionId", "url", "maxBytes"],
        ),
        "upload_files" => (
            json!({
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
                "sessionId": id(),
                "pageId": id(),
                "expression": string(1, MAX_HTML_BYTES),
                "timeoutMs": timeout_ms(),
                "awaitPromise": {"type":"boolean"}
            }),
            vec!["sessionId", "pageId", "expression"],
        ),
        // Intent surface: agents submit `{ kind: "intent", input: { kind: "locate"|… } }`
        // inside CommandEnvelope via this tool (no dedicated intent_* tools).
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
        "AccessibilityNode": accessibility_node(0),
        "WaitForCommand": wait_for_command(),
        "TargetSpec": target_spec(),
        "TextMatch": {"oneOf":[
            tagged_content("exact", string(0, MAX_STRING_BYTES)),
            tagged_content("contains", string(0, MAX_STRING_BYTES)),
            tagged_content("regex", string(0, MAX_STRING_BYTES))
        ]},
        "WaitCondition": {"oneOf": wait_conditions()},
        "ScreenshotMode": {"oneOf": screenshot_modes()},
        "Evidence": {"oneOf": evidence_variants()},
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
        "WorkflowCheckpoint": workflow_checkpoint()
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
                "evidence":array(json!({"$ref":"#/$defs/Evidence"}), MAX_EVIDENCE_ITEMS),
                "recoveryHistory":array(json!({"$ref":"#/$defs/RecoveryRecord"}), MAX_COLLECTION_ITEMS),
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
            "openPage",
            object(json!({"url":nullable(string(0, MAX_URL_BYTES))}), &["url"]),
        ),
        tagged_input("listPages", json!({"type":"null"})),
        tagged_input("closePage", object(json!({"pageId":id()}), &["pageId"])),
        tagged_input("activatePage", object(json!({"pageId":id()}), &["pageId"])),
        tagged_input(
            "accessibilitySnapshot",
            object(
                json!({"maxNodes":{"type":"integer","minimum":1,"maximum":2048}}),
                &[],
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
                "excludedClasses":array(string(1, MAX_STRING_BYTES), MAX_EXCLUDED_CLASSES)
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
            "intentExecution",
            json!({"record":{"$ref":"#/$defs/ExecutionRecord"}}),
            &["record"],
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
fn error_code() -> Value {
    json!({"type":"string","enum":[
        "invalidRequest","notFound","deadlineExceeded","browserLaunchFailed",
        "browserCommandFailed","verificationFailed","journalFailed","resourceExhausted",
        "policyDenied","internal","targetNotFound","targetAmbiguous","frameNotFound",
        "shadowRootUnavailable","targetDetached","waitConditionTimedOut",
        "screenshotCaptureFailed","networkPolicyDenied","httpResponseTooLarge",
        "httpTransferFailed","httpStateConflict","httpEquivalenceUnproven",
        "intentCompileFailed","intentActionMismatch","obstructionSuspected",
        "visionAssistDenied","visionAssistFailed"
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
            json!({"checkpointId":id(),"lineage":object(json!({"workflowId":id(),"abandonedAttemptId":id(),"attemptId":id(),"reason":string(1,MAX_STRING_BYTES)}), &["workflowId","abandonedAttemptId","attemptId","reason"])}),
            &["checkpointId", "lineage"],
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

fn timeout_ms() -> Value {
    json!({"type":"integer","minimum":1,"maximum":MAX_TIMEOUT_MS})
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
fn nonempty_array(items: Value, max: usize) -> Value {
    json!({"type":"array","items":items,"minItems":1,"maxItems":max})
}
fn nullable(schema: Value) -> Value {
    json!({"oneOf":[schema,{"type":"null"}]})
}
/// An unconstrained schema (no `type`, `oneOf`, `const`, or `enum`) — `validate` accepts
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

fn accessibility_node(depth: usize) -> Value {
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
    if depth < 32 {
        schema["properties"]["children"] = array(accessibility_node(depth + 1), 256);
    }
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
