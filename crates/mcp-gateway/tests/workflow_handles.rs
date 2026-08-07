//! Advertised workflow-handle contracts stay independent from `tools/call`.

mod common;

use std::{sync::Arc, time::Duration as StdDuration};

use chrono::{Duration, Utc};
use interface_core::{AuthorityStore, CapabilityHandle};
use mcp_gateway::Server;
use serde_json::{json, Value};
use types::{Capability, CommandId, CommandPhase, PrincipalId};
use uuid::uuid;
use workflow_journal::CommandJournal;

use common::{initialize, live_server, request};

async fn live_with_capabilities(capabilities: Vec<Capability>) -> common::LiveServer {
    let handle = verified_handle(capabilities).await;
    let live = live_server(handle).await;
    initialize(&live.server).await;
    live
}

async fn verified_handle(capabilities: Vec<Capability>) -> CapabilityHandle {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000039")),
            capabilities,
            Utc::now() + Duration::hours(1),
        )
        .await
        .expect("issue capability token");
    authority
        .verify(&token.expose_once())
        .await
        .expect("verify token")
}

async fn call_tool(server: &Server, id: u64, name: &str, arguments: Value) -> Value {
    server
        .handle_message(request(
            id,
            "tools/call",
            json!({"name":name,"arguments":arguments}),
        ))
        .await
        .expect("tools/call response")
}

async fn start(server: &Server, id: u64, arguments: Value) -> Value {
    call_tool(server, id, "workflow_start", arguments).await
}

async fn observe(server: &Server, id: u64, arguments: Value) -> Value {
    call_tool(server, id, "workflow_observe", arguments).await
}

async fn cancel(server: &Server, request_id: u64) {
    let response = server
        .handle_message(json!({
            "jsonrpc":"2.0",
            "method":"notifications/cancelled",
            "params":{"requestId":request_id,"reason":"race fixture"}
        }))
        .await;
    assert!(response.is_none());
}

async fn wait_for_no_sessions(runtime: &sdk_core::RuntimeService) {
    tokio::time::timeout(StdDuration::from_secs(5), async {
        loop {
            if runtime.list_sessions().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached cleanup removes the session within five seconds");
}

async fn assert_navigation_terminal(live: &common::LiveServer, url: &str) {
    let records = live.journal.records().await;
    let command_id =
        records
            .iter()
            .find_map(|record| {
                let envelope = record.envelope.as_ref()?;
                match &envelope.command {
                    types::RuntimeCommand::Primitive(types::PrimitiveCommand::Navigate(
                        command,
                    )) if command.url == url => Some(envelope.command_id.clone()),
                    _ => None,
                }
            })
            .expect("recorded navigation command");
    let last = records
        .iter()
        .filter(|record| record.command_id == command_id)
        .last()
        .expect("navigation journal records");
    assert!(
        matches!(last.phase, CommandPhase::Completed | CommandPhase::Failed),
        "navigation journal stopped at {:?}",
        last.phase
    );
}

async fn advertised_tools() -> (common::LiveServer, Vec<Value>) {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    live.server
        .handle_message(request(
            1,
            "tools/call",
            json!({"name":"toolset_select","arguments":{"toolset":"full"}}),
        ))
        .await
        .expect("select full toolset");
    let response = live
        .server
        .handle_message(request(2, "tools/list", json!({})))
        .await
        .expect("tools/list response");
    let tools = response["result"]["tools"]
        .as_array()
        .expect("tools/list tools")
        .clone();
    (live, tools)
}

fn advertised_input<'a>(tools: &'a [Value], name: &str) -> &'a Value {
    &tools
        .iter()
        .find(|tool| tool["name"] == name)
        .unwrap_or_else(|| panic!("{name} is advertised"))["inputSchema"]
}

fn assert_accepts(schema: &Value, instance: Value) {
    let validator = jsonschema::validator_for(schema).expect("advertised schema compiles");
    assert!(
        validator.is_valid(&instance),
        "schema rejected {instance}: {schema}"
    );
}

fn assert_rejects(schema: &Value, instance: Value) {
    let validator = jsonschema::validator_for(schema).expect("advertised schema compiles");
    assert!(
        !validator.is_valid(&instance),
        "schema accepted {instance}: {schema}"
    );
}

fn assert_output_validates(schema: &Value, response: &Value, label: &str) {
    let validator = jsonschema::validator_for(schema).expect("advertised output schema compiles");
    let structured = &response["result"]["structuredContent"];
    if let Err(error) = validator.validate(structured) {
        panic!("{label} does not validate: {error}\nvalue: {structured}\nschema: {schema}");
    }
}

#[tokio::test]
async fn advertised_scope_schemas_accept_exactly_one_scope_form() {
    let (_live, tools) = advertised_tools().await;
    let navigate = advertised_input(&tools, "navigate");
    let context_ask = advertised_input(&tools, "context_ask");
    let handle = "wf_0123456789abcdef0123456789abcdef";
    let session = "10000000-0000-4000-8000-000000000001";
    let page = "10000000-0000-4000-8000-000000000002";
    let workflow = "10000000-0000-4000-8000-000000000003";

    assert_accepts(
        navigate,
        json!({"workflowHandle":handle,"url":"https://example.test/"}),
    );
    assert_accepts(
        navigate,
        json!({"sessionId":session,"pageId":page,"workflowId":workflow,"url":"https://example.test/"}),
    );
    assert_accepts(
        context_ask,
        json!({"workflowHandle":handle,"description":"Continue"}),
    );
    assert_accepts(
        context_ask,
        json!({"sessionId":session,"pageId":page,"description":"Continue"}),
    );
}

#[tokio::test]
async fn advertised_scope_schemas_reject_mixed_scope_and_keep_business_fields_required() {
    let (_live, tools) = advertised_tools().await;
    let navigate = advertised_input(&tools, "navigate");
    let intent_fill = advertised_input(&tools, "intent_fill");
    let handle = "wf_0123456789abcdef0123456789abcdef";
    let session = "10000000-0000-4000-8000-000000000001";
    let page = "10000000-0000-4000-8000-000000000002";
    let workflow = "10000000-0000-4000-8000-000000000003";

    for mixed in [
        json!({"workflowHandle":handle,"sessionId":session,"url":"https://example.test/"}),
        json!({"workflowHandle":handle,"pageId":page,"url":"https://example.test/"}),
        json!({"workflowHandle":handle,"workflowId":workflow,"url":"https://example.test/"}),
    ] {
        assert_rejects(navigate, mixed);
    }
    assert_rejects(navigate, json!({"workflowHandle":handle}));
    assert_rejects(
        intent_fill,
        json!({"workflowHandle":handle,"purpose":"Email"}),
    );
}

#[tokio::test]
async fn explicit_id_navigate_keeps_the_existing_mcp_result_shape() {
    let (live, _tools) = advertised_tools().await;
    let mut next_id = 3;
    let (session_id, page_id) = common::create_session_and_page(&live.server, &mut next_id).await;
    let workflow_id = "10000000-0000-4000-8000-000000000004";
    next_id += 1;
    let response = live
        .server
        .handle_message(request(
            next_id,
            "tools/call",
            json!({
                "name":"navigate",
                "arguments":{
                    "sessionId":session_id,
                    "pageId":page_id,
                    "workflowId":workflow_id,
                    "url":"https://example.test/",
                    "waitUntil":"interactive",
                    "timeoutMs":5000
                }
            }),
        ))
        .await
        .expect("navigate response");

    assert!(response.get("error").is_none(), "{response}");
    let structured = &response["result"]["structuredContent"];
    assert_eq!(structured["workflowId"], workflow_id);
    assert_eq!(
        response["result"]["content"][0]["text"],
        serde_json::to_string(structured).expect("structured result serializes")
    );
}

#[tokio::test]
async fn workflow_start_and_observe_are_advertised_and_callable() {
    let (live, tools) = advertised_tools().await;
    assert!(tools.iter().any(|tool| tool["name"] == "workflow_start"));
    assert!(tools.iter().any(|tool| tool["name"] == "workflow_observe"));
    let started = start(&live.server, 3, json!({"profile":"harness"})).await;
    assert_eq!(
        started["result"]["structuredContent"]["status"], "completed",
        "{started}"
    );
    let observed = observe(
        &live.server,
        4,
        json!({"workflowHandle":started["result"]["structuredContent"]["workflowHandle"]}),
    )
    .await;
    assert_eq!(
        observed["result"]["structuredContent"]["status"], "completed",
        "{observed}"
    );
}

#[tokio::test]
async fn workflow_observe_without_goal_returns_bound_live_accessibility_outcome() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let started = start(&live.server, 300, json!({"profile":"observe-live"})).await;
    let binding = &started["result"]["structuredContent"];
    let response = observe(
        &live.server,
        301,
        json!({"workflowHandle":binding["workflowHandle"]}),
    )
    .await;

    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["isError"], false, "{response}");
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["status"], "completed");
    assert_eq!(result["source"], "live");
    for field in ["workflowHandle", "sessionId", "pageId", "workflowId"] {
        assert_eq!(result[field], binding[field], "{field}: {response}");
    }
    assert_eq!(result["retainedAnswer"], Value::Null);
    assert_eq!(result["formSnapshot"], Value::Null);
    assert_eq!(result["observationOutcome"]["status"], "completed");
    assert_eq!(
        result["observationOutcome"]["workflowId"],
        binding["workflowId"]
    );
    let evidence = &result["observationOutcome"]["evidence"][0];
    assert_eq!(evidence["kind"], "accessibilitySnapshot");
    assert_eq!(evidence["pageId"], binding["pageId"]);
    assert_eq!(evidence["nodes"][0]["name"], "Email address");
    assert_eq!(
        evidence["nodes"][0]["target"]["accessibleName"],
        "Email address"
    );
    assert_eq!(live.accessibility_calls(), 1);
    assert_eq!(
        live.probe
            .last_accessibility_max_nodes
            .load(std::sync::atomic::Ordering::SeqCst),
        256
    );
}

#[tokio::test]
async fn workflow_observe_prefers_retained_context_and_can_include_bounded_forms() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let started = start(&live.server, 310, json!({"profile":"observe-retained"})).await;
    let handle = started["result"]["structuredContent"]["workflowHandle"].clone();

    let first = observe(
        &live.server,
        311,
        json!({"workflowHandle":handle,"goal":"Email address"}),
    )
    .await;
    assert_eq!(
        first["result"]["structuredContent"]["source"], "live",
        "{first}"
    );
    assert_eq!(live.accessibility_calls(), 1);

    let second = observe(
        &live.server,
        312,
        json!({"workflowHandle":handle,"goal":"Email address"}),
    )
    .await;
    let retained = &second["result"]["structuredContent"];
    assert_eq!(retained["status"], "completed", "{second}");
    assert_eq!(retained["source"], "retained", "{second}");
    assert_eq!(retained["retainedAnswer"]["target"]["role"], "textbox");
    assert_eq!(
        retained["retainedAnswer"]["target"]["accessibleName"],
        "Email address"
    );
    assert_eq!(retained["retainedAnswer"]["confidence"], 1.0);
    assert_eq!(retained["observationOutcome"], Value::Null);
    assert_eq!(retained["formSnapshot"], Value::Null);
    assert_eq!(
        live.accessibility_calls(),
        1,
        "retained hit submitted live work"
    );

    let with_forms = observe(
        &live.server,
        313,
        json!({
            "workflowHandle":handle,
            "goal":"Email address",
            "includeForms":true
        }),
    )
    .await;
    let result = &with_forms["result"]["structuredContent"];
    assert_eq!(result["source"], "retained", "{with_forms}");
    assert_eq!(result["formSnapshot"]["pageId"], result["pageId"]);
    assert_eq!(
        result["formSnapshot"]["schemaVersion"],
        types::FORM_SNAPSHOT_SCHEMA_VERSION
    );
    assert_eq!(result["formSnapshot"]["forms"], json!([]));
    assert_eq!(result["formSnapshot"]["unownedControls"], json!([]));
    assert_eq!(result["formSnapshot"]["truncated"], false);
    assert_eq!(live.accessibility_calls(), 1);
    assert_eq!(live.form_calls(), 1);
    assert_eq!(
        live.probe
            .last_form_max_controls
            .load(std::sync::atomic::Ordering::SeqCst),
        128
    );
}

#[tokio::test]
async fn workflow_observe_bounds_reject_before_handle_lookup_or_runtime_effects() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let started = start(&live.server, 320, json!({"profile":"observe-bounds"})).await;
    let handle = started["result"]["structuredContent"]["workflowHandle"].clone();
    let unicode_256 = "🙂".repeat(256);
    let accepted = observe(
        &live.server,
        321,
        json!({"workflowHandle":handle,"goal":unicode_256}),
    )
    .await;
    assert_eq!(
        accepted["result"]["structuredContent"]["source"], "live",
        "{accepted}"
    );
    assert_eq!(live.accessibility_calls(), 1);

    let unknown = "wf_ffffffffffffffffffffffffffffffff";
    for (id, arguments) in [
        (322, json!({"workflowHandle":unknown,"maxNodes":0})),
        (323, json!({"workflowHandle":unknown,"maxNodes":2049})),
        (324, json!({"workflowHandle":unknown,"maxControls":0})),
        (325, json!({"workflowHandle":unknown,"maxControls":513})),
        (
            326,
            json!({"workflowHandle":unknown,"goal":"a".repeat(257)}),
        ),
        (
            327,
            json!({"workflowHandle":unknown,"goal":"🙂".repeat(257)}),
        ),
    ] {
        let response = observe(&live.server, id, arguments).await;
        assert_eq!(response["error"]["code"], -32602, "{response}");
        assert_ne!(
            response["error"]["data"]["reason"], "unknownWorkflowHandle",
            "validation looked up the handle first: {response}"
        );
        assert_eq!(
            live.accessibility_calls(),
            1,
            "invalid input dispatched: {response}"
        );
        assert_eq!(live.form_calls(), 0, "invalid input read forms: {response}");
    }
}

#[tokio::test]
async fn workflow_observe_without_page_read_skips_retained_but_forms_fail_before_live_work() {
    let live = live_with_capabilities(vec![
        Capability::SessionRead,
        Capability::SessionWrite,
        Capability::PageWrite,
        Capability::BrowserMutate,
    ])
    .await;
    let started = start(&live.server, 330, json!({"profile":"observe-capabilities"})).await;
    let handle = started["result"]["structuredContent"]["workflowHandle"].clone();

    let live_response = observe(
        &live.server,
        331,
        json!({"workflowHandle":handle,"goal":"Email address"}),
    )
    .await;
    assert_eq!(
        live_response["result"]["structuredContent"]["source"], "live",
        "{live_response}"
    );
    assert_eq!(live.accessibility_calls(), 1);

    let denied = observe(
        &live.server,
        332,
        json!({"workflowHandle":handle,"includeForms":true}),
    )
    .await;
    assert_eq!(
        denied["error"]["data"]["interfaceError"]["code"], "missingCapability",
        "{denied}"
    );
    assert_eq!(
        denied["error"]["data"]["interfaceError"]["requiredCapability"], "page:read",
        "{denied}"
    );
    assert_eq!(
        live.accessibility_calls(),
        1,
        "denied forms call submitted accessibility"
    );
    assert_eq!(live.form_calls(), 0);
}

#[tokio::test]
async fn workflow_observe_mirrors_live_failure_and_never_reads_forms_after_it() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let started = start(&live.server, 340, json!({"profile":"observe-failure"})).await;
    live.probe
        .accessibility_failures_remaining
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let response = observe(
        &live.server,
        341,
        json!({
            "workflowHandle":started["result"]["structuredContent"]["workflowHandle"],
            "includeForms":true
        }),
    )
    .await;

    assert_eq!(response["result"]["isError"], true, "{response}");
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["status"], "failed", "{response}");
    assert_eq!(result["source"], "live");
    assert_eq!(result["retainedAnswer"], Value::Null);
    assert_eq!(result["observationOutcome"]["status"], "failed");
    assert_eq!(result["formSnapshot"], Value::Null);
    assert_eq!(live.accessibility_calls(), 1);
    assert_eq!(live.form_calls(), 0, "failed observation attempted forms");
}

#[tokio::test]
async fn workflow_observe_marks_restarted_live_outcome_as_error_without_forms() {
    let handle = verified_handle(Capability::ALL.to_vec()).await;
    let live = common::live_server_restarting_accessibility(handle).await;
    initialize(&live.server).await;
    let started = start(&live.server, 345, json!({"profile":"observe-restarted"})).await;
    let binding = &started["result"]["structuredContent"];
    let response = observe(
        &live.server,
        346,
        json!({
            "workflowHandle":binding["workflowHandle"],
            "includeForms":true
        }),
    )
    .await;

    let result = &response["result"]["structuredContent"];
    assert_eq!(result["status"], "restarted", "{response}");
    assert_eq!(result["observationOutcome"]["status"], "restarted");
    assert_eq!(
        result["observationOutcome"]["evidence"][0]["kind"],
        "accessibilitySnapshot"
    );
    assert_eq!(
        result["observationOutcome"]["evidence"][0]["pageId"],
        binding["pageId"]
    );
    assert_eq!(response["result"]["isError"], true, "{response}");
    assert_eq!(result["formSnapshot"], Value::Null);
    assert_eq!(live.accessibility_calls(), 1);
    assert_eq!(
        live.form_calls(),
        0,
        "restarted observation attempted forms"
    );
}

#[tokio::test]
async fn workflow_observe_propagates_form_errors_without_partial_success_claims() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let started = start(&live.server, 350, json!({"profile":"observe-form-error"})).await;
    live.probe
        .form_failures_remaining
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let response = observe(
        &live.server,
        351,
        json!({
            "workflowHandle":started["result"]["structuredContent"]["workflowHandle"],
            "includeForms":true
        }),
    )
    .await;

    assert!(response.get("error").is_some(), "{response}");
    assert!(
        response.get("result").is_none(),
        "partial result leaked: {response}"
    );
    assert_eq!(live.accessibility_calls(), 1);
    assert_eq!(live.form_calls(), 1);
}

#[tokio::test]
async fn workflow_observe_unknown_and_reinitialized_handles_never_reach_runtime() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let started = start(&live.server, 360, json!({"profile":"observe-stale"})).await;
    let handle = started["result"]["structuredContent"]["workflowHandle"].clone();

    let unknown = observe(
        &live.server,
        361,
        json!({"workflowHandle":"wf_ffffffffffffffffffffffffffffffff"}),
    )
    .await;
    assert_eq!(
        unknown["error"]["data"]["reason"], "unknownWorkflowHandle",
        "{unknown}"
    );
    assert_eq!(live.accessibility_calls(), 0);

    initialize(&live.server).await;
    let stale = observe(&live.server, 362, json!({"workflowHandle":handle})).await;
    assert_eq!(
        stale["error"]["data"]["reason"], "unknownWorkflowHandle",
        "{stale}"
    );
    assert_eq!(live.accessibility_calls(), 0);
    assert_eq!(live.form_calls(), 0);
}

#[tokio::test]
async fn workflow_observe_actual_result_variants_match_the_advertised_output_schema() {
    let (live, tools) = advertised_tools().await;
    let schema = &tools
        .iter()
        .find(|tool| tool["name"] == "workflow_observe")
        .expect("workflow_observe advertised")["outputSchema"];
    let started = start(&live.server, 370, json!({"profile":"observe-schema"})).await;
    let handle = started["result"]["structuredContent"]["workflowHandle"].clone();

    let live_result = observe(
        &live.server,
        371,
        json!({"workflowHandle":handle,"goal":"Email address"}),
    )
    .await;
    let retained = observe(
        &live.server,
        372,
        json!({"workflowHandle":handle,"goal":"Email address"}),
    )
    .await;
    let forms = observe(
        &live.server,
        373,
        json!({"workflowHandle":handle,"goal":"Email address","includeForms":true}),
    )
    .await;
    live.probe
        .accessibility_failures_remaining
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let failure = observe(&live.server, 374, json!({"workflowHandle":handle})).await;

    assert_output_validates(schema, &live_result, "live workflow_observe");
    assert_output_validates(schema, &retained, "retained workflow_observe");
    assert_output_validates(schema, &forms, "forms workflow_observe");
    assert_output_validates(schema, &failure, "failed workflow_observe");
}

#[tokio::test]
async fn workflow_start_without_url_returns_a_completed_retained_binding() {
    let live = live_with_capabilities(vec![
        Capability::SessionRead,
        Capability::SessionWrite,
        Capability::PageWrite,
    ])
    .await;

    let response = start(&live.server, 10, json!({"profile":"harness"})).await;
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["isError"], false, "{response}");
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["status"], "completed", "{response}");
    let handle = result["workflowHandle"].as_str().expect("opaque handle");
    assert!(handle.starts_with("wf_") && handle.len() == 35, "{handle}");
    for field in ["sessionId", "pageId", "workflowId"] {
        uuid::Uuid::parse_str(result[field].as_str().expect(field)).expect(field);
    }
    assert_eq!(result["session"]["id"], result["sessionId"]);
    assert_eq!(result["page"]["id"], result["pageId"]);
    assert_eq!(result["navigationOutcome"], Value::Null);
}

#[tokio::test]
async fn workflow_start_with_url_requires_browser_mutate_before_creating_a_session() {
    let live = live_with_capabilities(vec![
        Capability::SessionRead,
        Capability::SessionWrite,
        Capability::PageWrite,
    ])
    .await;

    let response = start(
        &live.server,
        20,
        json!({"profile":"harness","url":"https://live-harness.test/"}),
    )
    .await;
    assert_eq!(
        response["error"]["data"]["interfaceError"]["code"], "missingCapability",
        "{response}"
    );
    let listed = call_tool(&live.server, 21, "session_list", json!({})).await;
    assert_eq!(
        listed["result"]["structuredContent"]["sessions"],
        json!([]),
        "{listed}"
    );
}

#[tokio::test]
async fn navigated_workflow_start_uses_the_minted_workflow_id_and_completes() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let response = start(
        &live.server,
        30,
        json!({"profile":"harness","url":"https://live-harness.test/"}),
    )
    .await;
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["status"], "completed", "{response}");
    assert_eq!(result["navigationOutcome"]["status"], "completed");

    let command_id = CommandId(
        uuid::Uuid::parse_str(
            result["navigationOutcome"]["commandId"]
                .as_str()
                .expect("navigation command id"),
        )
        .unwrap(),
    );
    let scan = live.journal.history(command_id).await.unwrap();
    let accepted = scan
        .records
        .iter()
        .find(|record| record.phase == CommandPhase::Accepted)
        .expect("accepted navigation record");
    assert_eq!(
        json!(accepted.envelope.as_ref().unwrap().workflow_id),
        result["workflowId"]
    );
    assert_eq!(scan.records.last().unwrap().phase, CommandPhase::Completed);
}

#[tokio::test]
async fn failed_navigation_returns_cleanup_evidence_and_no_handle_or_session() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let response = start(
        &live.server,
        40,
        json!({"profile":"harness","url":"https://live-harness.test/fail"}),
    )
    .await;
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["status"], "failed", "{response}");
    assert_eq!(response["result"]["isError"], true, "{response}");
    assert_eq!(result["reason"], "navigationFailed");
    assert_eq!(result["workflowHandle"], Value::Null);
    assert_eq!(result["navigationOutcome"]["status"], "failed");
    assert_eq!(result["pageClosed"], true);
    assert_eq!(result["sessionDeleted"], true);
    let listed = call_tool(&live.server, 41, "session_list", json!({})).await;
    assert_eq!(listed["result"]["structuredContent"]["sessions"], json!([]));
}

#[tokio::test]
async fn returned_handle_drives_primitives_intents_context_and_network_through_normalization() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let started = start(&live.server, 50, json!({"profile":"harness"})).await;
    let start_result = &started["result"]["structuredContent"];
    let handle = start_result["workflowHandle"].as_str().unwrap();
    let workflow_id = start_result["workflowId"].clone();

    let navigated = call_tool(
        &live.server,
        51,
        "navigate",
        json!({"workflowHandle":handle,"url":"https://live-harness.test/next"}),
    )
    .await;
    assert_eq!(
        navigated["result"]["structuredContent"]["workflowId"], workflow_id,
        "{navigated}"
    );

    let located = call_tool(
        &live.server,
        52,
        "intent_locate",
        json!({"workflowHandle":handle,"purpose":"Continue"}),
    )
    .await;
    assert_eq!(
        located["result"]["structuredContent"]["workflowId"], workflow_id,
        "{located}"
    );

    let context = call_tool(
        &live.server,
        53,
        "context_ask",
        json!({"workflowHandle":handle,"description":"Continue"}),
    )
    .await;
    assert_eq!(
        context["result"]["structuredContent"]["answer"],
        Value::Null,
        "{context}"
    );

    let network = call_tool(
        &live.server,
        54,
        "network_log",
        json!({"workflowHandle":handle,"clear":true}),
    )
    .await;
    assert_eq!(
        network["result"]["structuredContent"]["workflowId"], workflow_id,
        "{network}"
    );
}

#[tokio::test]
async fn workflow_handle_conflicts_and_unknown_handles_fail_before_dispatch() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let started = start(&live.server, 60, json!({"profile":"harness"})).await;
    let result = &started["result"]["structuredContent"];
    let handle = result["workflowHandle"].as_str().unwrap();

    for explicit in [
        json!({"sessionId":result["sessionId"]}),
        json!({"pageId":result["pageId"]}),
        json!({"workflowId":result["workflowId"]}),
    ] {
        let mut arguments = json!({
            "workflowHandle":handle,
            "url":"https://live-harness.test/"
        });
        arguments
            .as_object_mut()
            .unwrap()
            .extend(explicit.as_object().unwrap().clone());
        let response = call_tool(&live.server, 61, "navigate", arguments).await;
        assert_eq!(response["error"]["code"], -32602, "{response}");
        assert_eq!(
            response["error"]["data"]["reason"], "workflowBindingConflict",
            "{response}"
        );
    }

    let unknown = call_tool(
        &live.server,
        62,
        "navigate",
        json!({
            "workflowHandle":"wf_ffffffffffffffffffffffffffffffff",
            "url":"https://live-harness.test/"
        }),
    )
    .await;
    assert_eq!(unknown["error"]["code"], -32602, "{unknown}");
    assert_eq!(unknown["error"]["data"]["reason"], "unknownWorkflowHandle");
}

#[tokio::test]
async fn accepted_reinitialize_invalidates_handles_but_rejected_reinitialize_does_not() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let started = start(&live.server, 70, json!({"profile":"harness"})).await;
    let handle = started["result"]["structuredContent"]["workflowHandle"]
        .as_str()
        .unwrap();

    let rejected = live
        .server
        .handle_message(request(
            71,
            "initialize",
            json!({
                "protocolVersion":"invalid",
                "capabilities":{},
                "clientInfo":{"name":"harness","version":"1"}
            }),
        ))
        .await
        .unwrap();
    assert_eq!(rejected["error"]["code"], -32602, "{rejected}");
    let still_usable = call_tool(
        &live.server,
        72,
        "navigate",
        json!({"workflowHandle":handle,"url":"https://live-harness.test/one"}),
    )
    .await;
    assert_eq!(
        still_usable["result"]["structuredContent"]["status"],
        "completed"
    );

    initialize(&live.server).await;
    let invalidated = call_tool(
        &live.server,
        73,
        "navigate",
        json!({"workflowHandle":handle,"url":"https://live-harness.test/two"}),
    )
    .await;
    assert_eq!(
        invalidated["error"]["data"]["reason"], "unknownWorkflowHandle",
        "{invalidated}"
    );
}

#[tokio::test]
async fn workflow_handles_are_server_local_even_for_the_same_runtime_and_principal() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let other = Arc::new(Server::new(Arc::new(sdk_core::AuthenticatedRuntime::new(
        live.runtime.clone(),
        live.handle.clone(),
    ))));
    initialize(&other).await;
    let started = start(&live.server, 80, json!({"profile":"harness"})).await;
    let handle = started["result"]["structuredContent"]["workflowHandle"]
        .as_str()
        .unwrap();

    let response = call_tool(
        &other,
        81,
        "navigate",
        json!({"workflowHandle":handle,"url":"https://live-harness.test/"}),
    )
    .await;
    assert_eq!(
        response["error"]["data"]["reason"], "unknownWorkflowHandle",
        "{response}"
    );
}

#[tokio::test]
async fn boundary_click_through_a_handle_returns_checkpoint_and_binding_workflow_id() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let started = start(&live.server, 90, json!({"profile":"harness"})).await;
    let start_result = &started["result"]["structuredContent"];
    let response = call_tool(
        &live.server,
        91,
        "click",
        json!({
            "workflowHandle":start_result["workflowHandle"],
            "selector":"#go",
            "boundary":true,
            "autoCheckpoint":true
        }),
    )
    .await;
    let result = &response["result"]["structuredContent"];
    assert!(result["checkpointId"].is_string(), "{response}");
    assert_eq!(
        result["workflowId"], start_result["workflowId"],
        "{response}"
    );
}

#[tokio::test]
async fn successful_page_and_session_close_invalidate_only_affected_handles() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let first = start(&live.server, 100, json!({"profile":"first"})).await;
    let second = start(&live.server, 101, json!({"profile":"second"})).await;
    let first_result = &first["result"]["structuredContent"];
    let second_result = &second["result"]["structuredContent"];

    let closed_page = call_tool(
        &live.server,
        102,
        "page_close",
        json!({"workflowHandle":first_result["workflowHandle"]}),
    )
    .await;
    assert_eq!(
        closed_page["result"]["structuredContent"]["status"],
        "completed"
    );
    let first_unknown = call_tool(
        &live.server,
        103,
        "navigate",
        json!({"workflowHandle":first_result["workflowHandle"],"url":"https://live-harness.test/"}),
    )
    .await;
    assert_eq!(
        first_unknown["error"]["data"]["reason"],
        "unknownWorkflowHandle"
    );

    let second_usable = call_tool(
        &live.server,
        104,
        "navigate",
        json!({"workflowHandle":second_result["workflowHandle"],"url":"https://live-harness.test/"}),
    )
    .await;
    assert_eq!(
        second_usable["result"]["structuredContent"]["status"],
        "completed"
    );

    let closed_session = call_tool(
        &live.server,
        105,
        "session_close",
        json!({"sessionId":second_result["sessionId"]}),
    )
    .await;
    assert_eq!(
        closed_session["result"]["structuredContent"]["closed"],
        true
    );
    let second_unknown = call_tool(
        &live.server,
        106,
        "navigate",
        json!({"workflowHandle":second_result["workflowHandle"],"url":"https://live-harness.test/"}),
    )
    .await;
    assert_eq!(
        second_unknown["error"]["data"]["reason"],
        "unknownWorkflowHandle"
    );
}

#[tokio::test]
async fn next_start_reconciles_sessions_closed_by_another_server_without_reading_page_ids() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let other = Arc::new(Server::new(Arc::new(sdk_core::AuthenticatedRuntime::new(
        live.runtime.clone(),
        live.handle.clone(),
    ))));
    initialize(&other).await;
    let first = start(&live.server, 110, json!({"profile":"first"})).await;
    let first_result = &first["result"]["structuredContent"];
    let closed = call_tool(
        &other,
        111,
        "session_close",
        json!({"sessionId":first_result["sessionId"]}),
    )
    .await;
    assert_eq!(closed["result"]["structuredContent"]["closed"], true);

    let replacement = start(&live.server, 112, json!({"profile":"replacement"})).await;
    assert_eq!(
        replacement["result"]["structuredContent"]["status"],
        "completed"
    );
    let unknown = call_tool(
        &live.server,
        113,
        "navigate",
        json!({"workflowHandle":first_result["workflowHandle"],"url":"https://live-harness.test/"}),
    )
    .await;
    assert_eq!(unknown["error"]["data"]["reason"], "unknownWorkflowHandle");
}

#[tokio::test]
async fn external_page_close_leaves_the_handle_resolvable_until_normal_runtime_not_found() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let other = Arc::new(Server::new(Arc::new(sdk_core::AuthenticatedRuntime::new(
        live.runtime.clone(),
        live.handle.clone(),
    ))));
    initialize(&other).await;
    let started = start(&live.server, 120, json!({"profile":"harness"})).await;
    let result = &started["result"]["structuredContent"];
    let closed = call_tool(
        &other,
        121,
        "page_close",
        json!({
            "sessionId":result["sessionId"],
            "pageId":result["pageId"],
            "workflowId":result["workflowId"]
        }),
    )
    .await;
    assert_eq!(closed["result"]["structuredContent"]["status"], "completed");

    let response = call_tool(
        &live.server,
        122,
        "navigate",
        json!({"workflowHandle":result["workflowHandle"],"url":"https://live-harness.test/"}),
    )
    .await;
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["structuredContent"]["status"], "failed");
    assert_eq!(
        response["result"]["structuredContent"]["error"]["code"], "notFound",
        "{response}"
    );
}

#[tokio::test]
async fn explicit_session_close_reclaims_the_workflow_page_runtime_entry() {
    let live = live_with_capabilities(vec![
        Capability::SessionRead,
        Capability::SessionWrite,
        Capability::PageWrite,
    ])
    .await;
    let started = start(&live.server, 130, json!({"profile":"harness"})).await;
    let result = &started["result"]["structuredContent"];
    let page_id = types::PageId(uuid::Uuid::parse_str(result["pageId"].as_str().unwrap()).unwrap());
    assert!(live.runtime.pages.get(&page_id).await.is_ok());

    let closed = call_tool(
        &live.server,
        131,
        "session_close",
        json!({"sessionId":result["sessionId"]}),
    )
    .await;
    assert_eq!(closed["result"]["structuredContent"]["closed"], true);
    assert!(matches!(
        live.runtime.pages.get(&page_id).await,
        Err(types::RuntimeError::NotFound(_))
    ));
}

#[tokio::test]
async fn cancelling_while_open_page_is_blocked_keeps_setup_supervised_and_deletes_the_session() {
    let handle = verified_handle(vec![
        Capability::SessionRead,
        Capability::SessionWrite,
        Capability::PageWrite,
    ])
    .await;
    let live = common::live_server_blocking_open(handle).await;
    initialize(&live.server).await;
    let server = Arc::clone(&live.server);
    let request =
        tokio::spawn(async move { start(&server, 200, json!({"profile":"blocked-open"})).await });

    tokio::time::timeout(
        StdDuration::from_secs(5),
        live.probe.open_entered.notified(),
    )
    .await
    .expect("open_page entered within five seconds");
    cancel(&live.server, 200).await;
    live.probe.open_release.notify_one();

    let response = tokio::time::timeout(StdDuration::from_secs(5), request)
        .await
        .expect("cancelled request completed")
        .unwrap();
    assert_eq!(
        response["error"]["message"], "Request cancelled",
        "{response}"
    );
    wait_for_no_sessions(&live.runtime).await;
    let page_id = live
        .probe
        .opened_page
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .expect("blocked open records its page id");
    assert!(matches!(
        live.runtime.pages.get(&page_id).await,
        Err(types::RuntimeError::NotFound(_))
    ));
    assert_eq!(
        live.probe
            .worker_closes
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "session cleanup releases exactly one worker"
    );
}

#[tokio::test]
async fn cancelling_blocked_navigation_reaches_terminal_journal_phase_before_session_cleanup() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let server = Arc::clone(&live.server);
    let request = tokio::spawn(async move {
        start(
            &server,
            210,
            json!({
                "profile":"blocked-navigation",
                "url":"https://live-harness.test/block"
            }),
        )
        .await
    });

    tokio::time::timeout(
        StdDuration::from_secs(5),
        live.probe.navigation_entered.notified(),
    )
    .await
    .expect("navigation entered within five seconds");
    cancel(&live.server, 210).await;
    live.probe.navigation_release.notify_one();

    let response = tokio::time::timeout(StdDuration::from_secs(5), request)
        .await
        .expect("cancelled request completed")
        .unwrap();
    assert_eq!(
        response["error"]["message"], "Request cancelled",
        "{response}"
    );
    wait_for_no_sessions(&live.runtime).await;
    assert_navigation_terminal(&live, "https://live-harness.test/block").await;
}

#[tokio::test]
async fn reinitialize_during_blocked_navigation_finishes_journal_then_cleans_changed_generation() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let server = Arc::clone(&live.server);
    let request = tokio::spawn(async move {
        start(
            &server,
            220,
            json!({
                "profile":"generation-race",
                "url":"https://live-harness.test/block"
            }),
        )
        .await
    });

    tokio::time::timeout(
        StdDuration::from_secs(5),
        live.probe.navigation_entered.notified(),
    )
    .await
    .expect("navigation entered within five seconds");
    initialize(&live.server).await;
    live.probe.navigation_release.notify_one();

    let response = tokio::time::timeout(StdDuration::from_secs(5), request)
        .await
        .expect("generation-raced start completed")
        .unwrap();
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["status"], "failed", "{response}");
    assert_eq!(result["reason"], "workflowGenerationChanged", "{response}");
    assert_eq!(result["workflowHandle"], Value::Null, "{response}");
    assert_eq!(result["sessionDeleted"], true, "{response}");
    wait_for_no_sessions(&live.runtime).await;
    assert_navigation_terminal(&live, "https://live-harness.test/block").await;
}

#[tokio::test]
async fn generation_change_wins_after_blocked_navigation_reaches_a_failed_terminal_outcome() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let server = Arc::clone(&live.server);
    let request = tokio::spawn(async move {
        start(
            &server,
            221,
            json!({
                "profile":"generation-failed-navigation-race",
                "url":"https://live-harness.test/block-fail"
            }),
        )
        .await
    });

    tokio::time::timeout(
        StdDuration::from_secs(5),
        live.probe.navigation_entered.notified(),
    )
    .await
    .expect("navigation entered within five seconds");
    initialize(&live.server).await;
    live.probe.navigation_release.notify_one();

    let response = tokio::time::timeout(StdDuration::from_secs(5), request)
        .await
        .expect("generation-raced failed navigation completed")
        .unwrap();
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["status"], "failed", "{response}");
    assert_eq!(result["reason"], "workflowGenerationChanged", "{response}");
    assert_eq!(result["workflowHandle"], Value::Null, "{response}");
    assert_eq!(
        result["navigationOutcome"]["status"], "failed",
        "{response}"
    );
    assert_eq!(result["sessionDeleted"], true, "{response}");
    wait_for_no_sessions(&live.runtime).await;
    assert_navigation_terminal(&live, "https://live-harness.test/block-fail").await;
}

#[tokio::test]
async fn cancellation_racing_page_open_failure_still_performs_one_session_delete_attempt() {
    let handle = verified_handle(vec![
        Capability::SessionRead,
        Capability::SessionWrite,
        Capability::PageWrite,
    ])
    .await;
    let live = common::live_server_failing_open(handle).await;
    initialize(&live.server).await;
    let server = Arc::clone(&live.server);
    let request =
        tokio::spawn(async move { start(&server, 230, json!({"profile":"failing-open"})).await });

    tokio::time::timeout(
        StdDuration::from_secs(5),
        live.probe.open_entered.notified(),
    )
    .await
    .expect("failing open entered within five seconds");
    cancel(&live.server, 230).await;
    live.probe.open_release.notify_one();

    let response = tokio::time::timeout(StdDuration::from_secs(5), request)
        .await
        .expect("cancelled request completed")
        .unwrap();
    assert_eq!(
        response["error"]["message"], "Request cancelled",
        "{response}"
    );
    wait_for_no_sessions(&live.runtime).await;
    assert_eq!(
        live.probe
            .worker_closes
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the supervisor attempts worker/session deletion once"
    );
}

#[tokio::test]
async fn cancellation_while_delete_session_is_blocked_does_not_drop_cleanup_future() {
    let handle = verified_handle(Capability::ALL.to_vec()).await;
    let live = common::live_server_blocking_delete(handle).await;
    initialize(&live.server).await;
    let server = Arc::clone(&live.server);
    let request = tokio::spawn(async move {
        start(
            &server,
            240,
            json!({
                "profile":"blocked-delete",
                "url":"https://live-harness.test/fail"
            }),
        )
        .await
    });

    tokio::time::timeout(
        StdDuration::from_secs(5),
        live.probe.delete_entered.notified(),
    )
    .await
    .expect("delete_session entered within five seconds");
    cancel(&live.server, 240).await;
    live.probe.delete_release.notify_one();

    let response = tokio::time::timeout(StdDuration::from_secs(5), request)
        .await
        .expect("cancelled request completed")
        .unwrap();
    assert_eq!(
        response["error"]["message"], "Request cancelled",
        "{response}"
    );
    wait_for_no_sessions(&live.runtime).await;
    assert_eq!(
        live.probe
            .worker_closes
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "blocked cleanup is resumed exactly once"
    );
}

#[tokio::test]
async fn page_open_failure_returns_stable_bounded_fields_without_publishing_a_handle() {
    let handle = verified_handle(vec![
        Capability::SessionRead,
        Capability::SessionWrite,
        Capability::PageWrite,
    ])
    .await;
    let live = common::live_server_failing_open(handle).await;
    initialize(&live.server).await;
    let server = Arc::clone(&live.server);
    let request =
        tokio::spawn(async move { start(&server, 250, json!({"profile":"failing-open"})).await });
    tokio::time::timeout(
        StdDuration::from_secs(5),
        live.probe.open_entered.notified(),
    )
    .await
    .expect("failing open entered within five seconds");
    live.probe.open_release.notify_one();

    let response = tokio::time::timeout(StdDuration::from_secs(5), request)
        .await
        .expect("page-open failure response completed")
        .unwrap();
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["status"], "failed", "{response}");
    assert_eq!(result["reason"], "pageOpenFailed", "{response}");
    assert!(result["sessionId"].is_string(), "{response}");
    assert!(result["workflowId"].is_string(), "{response}");
    assert_eq!(result["session"]["id"], result["sessionId"], "{response}");
    assert_eq!(result["workflowHandle"], Value::Null, "{response}");
    assert_eq!(result["pageId"], Value::Null, "{response}");
    assert_eq!(result["page"], Value::Null, "{response}");
    assert_eq!(result["navigationOutcome"], Value::Null, "{response}");
    assert_eq!(result["pageClosed"], false, "{response}");
    assert_eq!(result["sessionDeleted"], true, "{response}");
    assert_eq!(result["cleanupErrorCode"], Value::Null, "{response}");
    wait_for_no_sessions(&live.runtime).await;
}

#[tokio::test]
async fn page_open_failure_with_failed_cleanup_still_never_publishes_a_handle() {
    let handle = verified_handle(vec![
        Capability::SessionRead,
        Capability::SessionWrite,
        Capability::PageWrite,
    ])
    .await;
    let live = common::live_server_failing_open_and_delete_once(handle).await;
    initialize(&live.server).await;
    let server = Arc::clone(&live.server);
    let request = tokio::spawn(async move {
        start(&server, 251, json!({"profile":"failing-open-and-cleanup"})).await
    });
    tokio::time::timeout(
        StdDuration::from_secs(5),
        live.probe.open_entered.notified(),
    )
    .await
    .expect("failing open entered within five seconds");
    live.probe.open_release.notify_one();

    let response = tokio::time::timeout(StdDuration::from_secs(5), request)
        .await
        .expect("page-open and cleanup failure response completed")
        .unwrap();
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["status"], "failed", "{response}");
    assert_eq!(result["reason"], "pageOpenFailed", "{response}");
    assert_eq!(result["workflowHandle"], Value::Null, "{response}");
    assert_eq!(result["pageId"], Value::Null, "{response}");
    assert_eq!(result["sessionDeleted"], false, "{response}");
    assert_eq!(result["cleanupErrorCode"], "internal", "{response}");
    assert_eq!(
        live.probe
            .worker_closes
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "failed page-open cleanup is attempted once"
    );
    assert_eq!(live.runtime.list_sessions().await.len(), 1);

    let repaired = call_tool(
        &live.server,
        252,
        "session_close",
        json!({"sessionId":result["sessionId"]}),
    )
    .await;
    assert_eq!(repaired["result"]["structuredContent"]["closed"], true);
    wait_for_no_sessions(&live.runtime).await;
}

#[tokio::test]
async fn workflow_start_real_success_and_all_terminal_failure_branches_match_output_schema() {
    let (_catalog_live, tools) = advertised_tools().await;
    let schema = &tools
        .iter()
        .find(|tool| tool["name"] == "workflow_start")
        .expect("workflow_start advertised")["outputSchema"];
    let validator = jsonschema::validator_for(schema).expect("workflow_start schema compiles");

    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let success = start(&live.server, 260, json!({"profile":"success"})).await;
    let navigation_failure = start(
        &live.server,
        261,
        json!({"profile":"navigation-failure","url":"https://live-harness.test/fail"}),
    )
    .await;

    let open_handle = verified_handle(Capability::ALL.to_vec()).await;
    let open_live = common::live_server_failing_open(open_handle).await;
    initialize(&open_live.server).await;
    let open_server = Arc::clone(&open_live.server);
    let open_request =
        tokio::spawn(
            async move { start(&open_server, 262, json!({"profile":"open-failure"})).await },
        );
    tokio::time::timeout(
        StdDuration::from_secs(5),
        open_live.probe.open_entered.notified(),
    )
    .await
    .unwrap();
    open_live.probe.open_release.notify_one();
    let open_failure = tokio::time::timeout(StdDuration::from_secs(5), open_request)
        .await
        .unwrap()
        .unwrap();

    let generation_live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let generation_server = Arc::clone(&generation_live.server);
    let generation_request = tokio::spawn(async move {
        start(
            &generation_server,
            263,
            json!({
                "profile":"generation-failure",
                "url":"https://live-harness.test/block"
            }),
        )
        .await
    });
    tokio::time::timeout(
        StdDuration::from_secs(5),
        generation_live.probe.navigation_entered.notified(),
    )
    .await
    .unwrap();
    initialize(&generation_live.server).await;
    generation_live.probe.navigation_release.notify_one();
    let generation_failure = tokio::time::timeout(StdDuration::from_secs(5), generation_request)
        .await
        .unwrap()
        .unwrap();

    for (label, response) in [
        ("success", success),
        ("pageOpenFailed", open_failure),
        ("navigationFailed", navigation_failure),
        ("workflowGenerationChanged", generation_failure),
    ] {
        let structured = &response["result"]["structuredContent"];
        if let Err(error) = validator.validate(structured) {
            panic!("{label} did not match workflow_start output schema: {error}; {structured}");
        }
    }
}

#[tokio::test]
async fn advertised_workflow_start_output_rejects_missing_extra_and_invalid_reason() {
    let (_catalog_live, tools) = advertised_tools().await;
    let schema = &tools
        .iter()
        .find(|tool| tool["name"] == "workflow_start")
        .expect("workflow_start advertised")["outputSchema"];
    let validator = jsonschema::validator_for(schema).expect("workflow_start schema compiles");

    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let success = start(
        &live.server,
        264,
        json!({"profile":"invalid-schema-success"}),
    )
    .await;
    let mut missing = success["result"]["structuredContent"].clone();
    missing
        .as_object_mut()
        .expect("success result is an object")
        .remove("workflowHandle");
    assert!(
        validator.validate(&missing).is_err(),
        "advertised schema accepted a success result missing workflowHandle: {missing}"
    );

    let mut extra = success["result"]["structuredContent"].clone();
    extra
        .as_object_mut()
        .expect("success result is an object")
        .insert("unexpected".into(), json!(true));
    assert!(
        validator.validate(&extra).is_err(),
        "advertised schema accepted an extra top-level field: {extra}"
    );

    let failure = start(
        &live.server,
        265,
        json!({
            "profile":"invalid-schema-failure",
            "url":"https://live-harness.test/fail"
        }),
    )
    .await;
    let mut invalid_reason = failure["result"]["structuredContent"].clone();
    invalid_reason["reason"] = json!("arbitraryFailure");
    assert!(
        validator.validate(&invalid_reason).is_err(),
        "advertised schema accepted a reason outside the closed enum: {invalid_reason}"
    );
}

#[tokio::test]
async fn intent_follow_description_preserves_boundary_no_retry_guidance() {
    let (_live, tools) = advertised_tools().await;
    let description = tools
        .iter()
        .find(|tool| tool["name"] == "intent_follow")
        .expect("intent_follow advertised")["description"]
        .as_str()
        .expect("intent_follow description is text");
    assert!(
        description.contains("Requires browser:mutate"),
        "{description}"
    );
    assert!(description.contains("On failure"), "{description}");
    assert!(description.contains("needsReconciliation"), "{description}");
    assert!(description.contains("do not retry"), "{description}");
    assert!(description.contains("recovery_status"), "{description}");
}

#[tokio::test]
async fn normal_failure_reports_one_failed_compensation_attempt_and_keeps_session_repairable() {
    let handle = verified_handle(Capability::ALL.to_vec()).await;
    let live = common::live_server_failing_delete_once(handle).await;
    initialize(&live.server).await;
    let response = start(
        &live.server,
        270,
        json!({
            "profile":"cleanup-failure",
            "url":"https://live-harness.test/fail"
        }),
    )
    .await;
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["status"], "failed", "{response}");
    assert_eq!(result["pageClosed"], true, "{response}");
    assert_eq!(result["sessionDeleted"], false, "{response}");
    assert_eq!(result["cleanupErrorCode"], "internal", "{response}");
    assert_eq!(
        live.probe
            .worker_closes
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "compensation is attempted once without an internal retry loop"
    );
    assert_eq!(live.runtime.list_sessions().await.len(), 1);

    let repaired = call_tool(
        &live.server,
        271,
        "session_close",
        json!({"sessionId":result["sessionId"]}),
    )
    .await;
    assert_eq!(repaired["result"]["structuredContent"]["closed"], true);
    wait_for_no_sessions(&live.runtime).await;
}

#[tokio::test]
async fn cancelled_failed_compensation_is_bounded_to_one_attempt_and_does_not_panic_or_retry() {
    let handle = verified_handle(Capability::ALL.to_vec()).await;
    let live = common::live_server_blocking_failing_delete_once(handle).await;
    initialize(&live.server).await;
    let server = Arc::clone(&live.server);
    let request = tokio::spawn(async move {
        start(
            &server,
            280,
            json!({
                "profile":"cancelled-cleanup-failure",
                "url":"https://live-harness.test/fail"
            }),
        )
        .await
    });
    tokio::time::timeout(
        StdDuration::from_secs(5),
        live.probe.delete_entered.notified(),
    )
    .await
    .expect("cleanup deletion entered within five seconds");
    cancel(&live.server, 280).await;
    live.probe.delete_release.notify_one();
    let response = tokio::time::timeout(StdDuration::from_secs(5), request)
        .await
        .expect("cancelled response completed")
        .unwrap();
    assert_eq!(
        response["error"]["message"], "Request cancelled",
        "{response}"
    );

    tokio::time::timeout(StdDuration::from_secs(5), async {
        loop {
            if live
                .probe
                .delete_failures_remaining
                .load(std::sync::atomic::Ordering::SeqCst)
                == 0
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed cleanup attempt completed");
    assert_eq!(
        live.probe
            .worker_closes
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "cancelled cleanup performs no hidden retry"
    );
    assert_eq!(live.runtime.list_sessions().await.len(), 1);

    live.probe.delete_release.notify_one();
    let session_id = live.runtime.list_sessions().await[0].id.clone();
    let repaired = call_tool(
        &live.server,
        281,
        "session_close",
        json!({"sessionId":session_id}),
    )
    .await;
    assert_eq!(repaired["result"]["structuredContent"]["closed"], true);
}

#[tokio::test]
async fn failed_authenticated_session_delete_preserves_page_runtime_until_successful_retry() {
    let handle = verified_handle(Capability::ALL.to_vec()).await;
    let live = common::live_server_failing_delete_once(handle).await;
    initialize(&live.server).await;
    let started = start(&live.server, 290, json!({"profile":"delete-retry"})).await;
    let result = &started["result"]["structuredContent"];
    let page_id = types::PageId(uuid::Uuid::parse_str(result["pageId"].as_str().unwrap()).unwrap());

    let failed = call_tool(
        &live.server,
        291,
        "session_close",
        json!({"sessionId":result["sessionId"]}),
    )
    .await;
    assert_eq!(
        failed["error"]["data"]["interfaceError"]["code"], "internal",
        "{failed}"
    );
    assert!(live.runtime.pages.get(&page_id).await.is_ok());
    assert_eq!(live.runtime.list_sessions().await.len(), 1);

    let retried = call_tool(
        &live.server,
        292,
        "session_close",
        json!({"sessionId":result["sessionId"]}),
    )
    .await;
    assert_eq!(retried["result"]["structuredContent"]["closed"], true);
    assert!(matches!(
        live.runtime.pages.get(&page_id).await,
        Err(types::RuntimeError::NotFound(_))
    ));
}
