//! Compact MCP evidence without changing journaled runtime outcomes.

use serde_json::Value;

pub(crate) fn compact_tool_response(response: &mut Value) {
    let Some(result) = response.get_mut("result") else {
        return;
    };
    let Some(outcome) = result.get_mut("structuredContent") else {
        return;
    };
    if compact_outcome(outcome, false) {
        // MCP clients may consume either representation. Keep the fallback
        // text and structured result identical; resource/image blocks stay.
        let text = outcome.to_string();
        if let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) {
            if let Some(block) = content.first_mut().filter(|block| block["type"] == "text") {
                block["text"] = Value::String(text);
            }
        }
    }
}

/// `nested` marks a runtime-owned page-state wrapper; a direct snapshot result
/// keeps every option.
fn compact_outcome(outcome: &mut Value, nested: bool) -> bool {
    // Failures and uncertain side effects need their complete repair evidence.
    if outcome.get("status").and_then(Value::as_str) != Some("completed") {
        return false;
    }
    let mut changed = false;
    if outcome.get("commandId").is_some() {
        if let Some(evidence) = outcome.get_mut("evidence").and_then(Value::as_array_mut) {
            evidence.retain_mut(|item| {
                match item.get("kind").and_then(Value::as_str) {
                    // A completed read may still have received an HTTP error.
                    // Preserve that status and its transport context for repair.
                    Some("executionPath")
                        if item
                            .get("status")
                            .and_then(Value::as_u64)
                            .is_some_and(|status| status >= 400) => {}
                    Some("executionPath" | "browserExecution" | "humanization") => {
                        changed = true;
                        return false;
                    }
                    Some("resolution") => {
                        if let Some(item) = item.as_object_mut() {
                            changed |= omit_candidate_details(item);
                        }
                    }
                    Some("accessibilitySnapshot") if nested => {
                        for node in item
                            .get_mut("nodes")
                            .and_then(Value::as_array_mut)
                            .into_iter()
                            .flatten()
                        {
                            changed |= collapse_combobox_options(node, false);
                        }
                    }
                    Some("intentExecution") => {
                        if let Some(record) = item.get_mut("record").and_then(Value::as_object_mut)
                        {
                            changed |= omit_candidate_details(record);
                            for field in ["purpose", "waitElapsedMs", "visionProposalSha256"] {
                                changed |= record.remove(field).is_some();
                            }
                        }
                    }
                    _ => {}
                }
                true
            });
        }
    }
    // These are runtime-owned wrappers, never arbitrary extracted/page data,
    // checkpoints, or events. Unknown evidence and nested payloads stay intact.
    for field in ["postState", "observationOutcome", "navigationOutcome"] {
        if let Some(child) = outcome.get_mut(field) {
            changed |= compact_outcome(child, true);
        }
    }
    changed
}

/// Option children kept per list inside a combobox; `selectOne` matches any
/// option by value or label, so the rest is counted in `optionsOmitted`.
const MAX_COMBOBOX_OPTIONS: usize = 10;

fn collapse_combobox_options(node: &mut Value, in_combobox: bool) -> bool {
    let Some(node) = node.as_object_mut() else {
        return false;
    };
    let in_combobox = in_combobox || node.get("role").and_then(Value::as_str) == Some("combobox");
    let Some(children) = node.get_mut("children").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut changed = false;
    let mut kept = 0;
    let mut omitted = 0usize;
    children.retain_mut(|child| {
        if !in_combobox || child.get("role").and_then(Value::as_str) != Some("option") {
            changed |= collapse_combobox_options(child, in_combobox);
            return true;
        }
        kept += 1;
        omitted += usize::from(kept > MAX_COMBOBOX_OPTIONS);
        kept <= MAX_COMBOBOX_OPTIONS
    });
    if omitted > 0 {
        node.insert("optionsOmitted".into(), Value::from(omitted));
    }
    changed || omitted > 0
}

fn omit_candidate_details(record: &mut serde_json::Map<String, Value>) -> bool {
    let Some(candidates) = record.get_mut("candidates").and_then(Value::as_array_mut) else {
        return false;
    };
    if candidates.is_empty() {
        return false;
    }
    // Keep the required array for existing typed MCP consumers. The marker
    // distinguishes withheld diagnostic details from an observed empty list.
    candidates.clear();
    record.insert("candidateDetailsOmitted".into(), Value::Bool(true));
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn response(outcome: Value) -> Value {
        json!({"result":{
            "content":[{"type":"text","text":outcome.to_string()},
                {"type":"resource_link","uri":"artifact://proof"}],
            "structuredContent":outcome,"isError":false
        }})
    }

    fn completed() -> Value {
        let candidates = (0..32)
            .map(|index| {
                json!({
                    "role":"button","name":format!("Action {index}"),"score":100-index,
                    "reasons":["role match","name match","visible","enabled"]
                })
            })
            .collect::<Vec<_>>();
        json!({"status":"completed","commandId":"command-1","pageDerived":true,
            "workflowHandle":"wf_binding","postStateStatus":{"status":"unavailable","code":"readFailed","reconciliationRequired":true},
            "evidence":[
                {"kind":"executionPath","elapsedMs":34,"path":"chromium"},
                {"kind":"browserExecution","engine":"chromium","browserVersion":"155","profileId":"profile","interactionPath":"cdp"},
                {"kind":"humanization","engine":"chromium","actions":4,"synthesizedMs":120},
                {"kind":"resolution","target":{"role":"button","accessibleName":"Continue","ordinal":2,"framePath":[{"role":"iframe","accessibleName":"Billing"}]},"fingerprint":{"pageId":"page-1"},"candidates":candidates,"bestMatchAuthorized":false},
                {"kind":"intentExecution","record":{"intentKind":"follow","purpose":"Continue","resolutionPath":"deterministic","planSummary":"Resolve, activate, and verify the control","candidates":candidates,"waitElapsedMs":4,"verification":"matchedExpectedState","artifactIds":["proof"],"visionProposalSha256":null}},
                {"kind":"wait","condition":{"kind":"url"},"observed":"https://example.test/next","elapsedMs":4,"observations":2},
                {"kind":"extraction","field":"account","errorCode":"targetNotFound"},
                {"kind":"popupClosed","openerPageId":"opener","popupPageId":"popup"},
                {"kind":"futureEvidence","data":{"candidates":["keep"]}}
        ]})
    }

    #[test]
    fn compact_success_keeps_decisions_and_reduces_response_bytes() {
        let raw = completed();
        let mut wire = response(raw.clone());
        let before = wire.to_string().len();
        compact_tool_response(&mut wire);
        let compact = &wire["result"]["structuredContent"];
        assert!(
            wire.to_string().len() * 4 < before,
            "compact response must remove ranking duplication"
        );
        assert_eq!(compact["commandId"], raw["commandId"]);
        assert_eq!(compact["workflowHandle"], raw["workflowHandle"]);
        assert_eq!(compact["postStateStatus"], raw["postStateStatus"]);
        let evidence = compact["evidence"].as_array().unwrap();
        assert_eq!(evidence.len(), 6);
        assert_eq!(evidence[0]["target"], raw["evidence"][3]["target"]);
        assert_eq!(evidence[0]["bestMatchAuthorized"], false);
        assert_eq!(evidence[0]["candidates"], json!([]));
        assert_eq!(evidence[0]["candidateDetailsOmitted"], true);
        assert_eq!(
            evidence[1]["record"]["verification"],
            "matchedExpectedState"
        );
        assert_eq!(evidence[1]["record"]["artifactIds"], json!(["proof"]));
        assert_eq!(
            evidence[1]["record"]["planSummary"],
            raw["evidence"][4]["record"]["planSummary"]
        );
        assert_eq!(evidence[1]["record"]["candidateDetailsOmitted"], true);
        assert_eq!(&evidence[2..], &raw["evidence"].as_array().unwrap()[5..]);
        assert_eq!(
            serde_json::from_str::<Value>(wire["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap(),
            *compact
        );
        assert_eq!(wire["result"]["content"][1]["uri"], "artifact://proof");
        eprintln!("response bytes: {before} -> {}", wire.to_string().len());
    }

    #[test]
    fn compact_success_collapses_long_option_lists_inside_a_combobox() {
        let options = |count: usize| {
            (0..count)
                .map(|index| {
                    let label = format!("{}", 1900 + index);
                    json!({"role":"option","target":{"role":"option","accessibleName":label}})
                })
                .collect::<Vec<_>>()
        };
        let snapshot = json!({"kind":"accessibilitySnapshot","pageId":"page-1","truncated":false,"nodes":[
            {"role":"combobox","target":{"role":"combobox","accessibleName":"Year"},"value":"1950",
             "children":[{"role":"MenuListPopup","children":options(120)}]},
            {"role":"listbox","target":{"role":"listbox","accessibleName":"Pick one"},"children":options(20)}
        ]});
        let post_state =
            json!({"status":"completed","commandId":"command-2","evidence":[snapshot.clone()]});
        let raw = json!({"status":"completed","commandId":"command-1",
            "evidence":[snapshot],"postState":post_state});
        let mut wire = response(raw.clone());
        compact_tool_response(&mut wire);
        let compact = &wire["result"]["structuredContent"];
        assert_eq!(compact["evidence"], raw["evidence"]);
        let nodes = &compact["postState"]["evidence"][0]["nodes"];
        let popup = &nodes[0]["children"][0];
        assert_eq!(popup["children"].as_array().unwrap().len(), 10);
        assert_eq!(popup["optionsOmitted"], 110);
        assert_eq!(
            popup["children"][0],
            raw["evidence"][0]["nodes"][0]["children"][0]["children"][0]
        );
        assert_eq!(nodes[0]["value"], "1950");
        assert_eq!(nodes[1], raw["evidence"][0]["nodes"][1]);
        assert_eq!(
            serde_json::from_str::<Value>(wire["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap(),
            wire["result"]["structuredContent"]
        );
    }

    #[test]
    fn every_non_completed_outcome_keeps_all_failure_evidence() {
        for status in [
            "failed",
            "retryableFailure",
            "needsReconciliation",
            "policyDenied",
            "resourceExhausted",
            "restarted",
        ] {
            let mut outcome = completed();
            outcome["status"] = json!(status);
            outcome["error"] =
                json!({"code":"verificationFailed","repair":{"action":"recovery_status"}});
            let raw = response(outcome);
            let mut wire = raw.clone();
            compact_tool_response(&mut wire);
            assert_eq!(wire, raw, "{status}");

            let mut parent = response(
                json!({"status":"completed","postState":raw["result"]["structuredContent"]}),
            );
            let original = parent.clone();
            compact_tool_response(&mut parent);
            assert_eq!(parent, original, "nested {status}");
        }
    }

    #[test]
    fn completed_reads_keep_http_error_status_and_partial_data() {
        let evidence = json!([
            {"kind":"executionPath","path":"directHttp","status":429,"elapsedMs":10},
            {"kind":"structuredExtraction","value":{"partial":true},"truncated":true}
        ]);
        let raw = response(json!({"status":"completed","commandId":"read-1","evidence":evidence}));
        let mut wire = raw.clone();
        compact_tool_response(&mut wire);
        assert_eq!(wire, raw);
    }

    #[test]
    fn only_known_outcome_wrappers_are_traversed() {
        let page_data = completed();
        let mut parent = completed();
        parent["evidence"] = json!([
            {"kind":"javaScriptResult","value":page_data,"truncated":false},
            {"kind":"structuredExtraction","value":page_data,"truncated":false}
        ]);
        parent["postState"] =
            json!({"status":"completed","observationOutcome":completed(),"formSnapshot":page_data});
        let mut wire = response(parent.clone());
        compact_tool_response(&mut wire);
        let compact = &wire["result"]["structuredContent"];
        assert_eq!(compact["evidence"], parent["evidence"]);
        assert_eq!(compact["postState"]["formSnapshot"], page_data);
        assert_eq!(
            compact["postState"]["observationOutcome"]["evidence"]
                .as_array()
                .unwrap()
                .len(),
            6
        );
        let mut diagnostic = response(json!({"checkpoint":page_data,"events":[page_data]}));
        let original = diagnostic.clone();
        compact_tool_response(&mut diagnostic);
        assert_eq!(diagnostic, original);
    }
}
