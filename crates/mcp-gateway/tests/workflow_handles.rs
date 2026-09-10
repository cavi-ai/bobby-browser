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

use common::{create_session_and_page, initialize, live_adaptive_server, live_server, request};

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

async fn download_fixture(body: &'static [u8]) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("download fixture binds");
    let address = listener.local_addr().expect("download fixture address");
    tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.expect("HTTP request arrives");
            let mut request = [0; 2048];
            let request_bytes = socket.read(&mut request).await.expect("HTTP request reads");
            assert!(request_bytes > 0, "HTTP request is not empty");
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/csv\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket
                .write_all(headers.as_bytes())
                .await
                .expect("HTTP headers write");
            socket.write_all(body).await.expect("HTTP body writes");
        }
    });
    format!("http://{address}/report.csv")
}

#[tokio::test]
async fn download_url_save_as_materializes_through_the_mcp_boundary() {
    let handle = verified_handle(vec![
        Capability::SessionWrite,
        Capability::PageWrite,
        Capability::BrowserMutate,
        Capability::FileDownload,
    ])
    .await;
    let live = live_adaptive_server(handle).await;
    initialize(&live.server).await;
    let mut next_id = 10;
    let (session_id, page_id) = create_session_and_page(&live.server, &mut next_id).await;
    let url = download_fixture(b"name,value\nlatency,70.2\n").await;
    let navigated = call_tool(
        &live.server,
        19,
        "navigate",
        json!({
            "sessionId": session_id.0.to_string(),
            "pageId": page_id.0.to_string(),
            "url": url
        }),
    )
    .await;
    assert_eq!(
        navigated["result"]["structuredContent"]["status"], "completed",
        "{navigated}"
    );

    let response = call_tool(
        &live.server,
        20,
        "download_url",
        json!({
            "sessionId": session_id.0.to_string(),
            "pageId": page_id.0.to_string(),
            "url": url,
            "expectedContentType": "text/csv",
            "maxBytes": 1024,
            "saveAs": "report.csv"
        }),
    )
    .await;

    assert_eq!(
        response["result"]["structuredContent"]["status"], "completed",
        "{response}"
    );
    assert_eq!(
        response["result"]["structuredContent"]["evidence"][0]["savedTo"], "report.csv",
        "relative saveAs must remain relative at the MCP boundary"
    );
    assert_eq!(
        tokio::fs::read(live.downloads_dir.join("report.csv"))
            .await
            .expect("saveAs output exists"),
        b"name,value\nlatency,70.2\n"
    );
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
        .rfind(|record| record.command_id == command_id)
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

    // Handle normalization now runs before argument validation, so an
    // unknown handle refuses as `unknownWorkflowHandle` (normalization's
    // own refusal) and never reaches the runtime; the same bounds on the
    // valid handle reject as plain `schemaViolation` with no runtime work.
    let unknown = "wf_ffffffffffffffffffffffffffffffff";
    let unknown_response = observe(
        &live.server,
        322,
        json!({"workflowHandle":unknown,"maxNodes":0}),
    )
    .await;
    assert_eq!(
        unknown_response["error"]["code"], -32602,
        "{unknown_response}"
    );
    assert_eq!(
        unknown_response["error"]["data"]["reason"], "unknownWorkflowHandle",
        "{unknown_response}"
    );
    assert_eq!(live.accessibility_calls(), 1);
    assert_eq!(live.form_calls(), 0);

    for (id, arguments) in [
        (323, json!({"workflowHandle":handle,"maxNodes":0})),
        (324, json!({"workflowHandle":handle,"maxNodes":2049})),
        (325, json!({"workflowHandle":handle,"maxControls":0})),
        (326, json!({"workflowHandle":handle,"maxControls":513})),
        (327, json!({"workflowHandle":handle,"goal":"a".repeat(257)})),
        (
            328,
            json!({"workflowHandle":handle,"goal":"🙂".repeat(257)}),
        ),
    ] {
        let response = observe(&live.server, id, arguments).await;
        assert_eq!(response["error"]["code"], -32602, "{response}");
        assert!(
            matches!(
                response["error"]["data"]["reason"].as_str(),
                Some("schemaViolation") | Some("malformedArguments")
            ),
            "bounds reject before any runtime effect: {response}"
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
    assert!(
        result["session"]["page_ids"]
            .as_array()
            .expect("page_ids array")
            .contains(&result["pageId"]),
        "session page_ids must contain the opened page id even without a navigation: {response}"
    );
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
    assert_eq!(
        result["page"]["url"],
        json!("https://live-harness.test/"),
        "page url must reflect the completed navigation, not the pre-navigation open: {response}"
    );
    assert_eq!(
        result["page"]["ready_state"],
        json!("interactive"),
        "page ready_state must reflect the completed navigation: {response}"
    );
    assert!(
        result["session"]["page_ids"]
            .as_array()
            .expect("page_ids array")
            .contains(&result["pageId"]),
        "session page_ids must contain the opened page id: {response}"
    );

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
    // A miss is spelled out: the agent reading structuredContent gets the
    // reason and the snapshot next step, not a bare null to interpret.
    assert_eq!(
        context["result"]["structuredContent"]["hit"],
        json!(false),
        "{context}"
    );
    assert_eq!(
        context["result"]["structuredContent"]["reason"], "notRemembered",
        "{context}"
    );
    assert_eq!(
        context["result"]["structuredContent"]["nextStep"], "a11y_snapshot",
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

/// The gauntlet-observed failure this closes: an agent calls a
/// `WORKFLOW_SCOPE_TOOLS` tool right after `workflow_start` but forgets the
/// handle entirely (no `workflowHandle`, no explicit ids). With exactly one
/// live binding on the connection, the call defaults to it instead of
/// bouncing off a schema rejection -- dispatched all the way to the fake
/// runtime, which fails this specific call on its own terms (no real target
/// named "Email" exists on the fresh page `workflow_start` opened, so
/// resolution reports `targetNotFound`). That failure is the proof: the
/// call was never rejected for a missing scope, and the outcome -- whatever
/// its status -- names the handle defaulting used, so the agent can see
/// what happened without reading `error.data`.
#[tokio::test]
async fn scope_less_intent_complete_form_defaults_to_the_only_live_handle_and_reports_it() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let started = start(&live.server, 80, json!({"profile":"harness"})).await;
    let handle = started["result"]["structuredContent"]["workflowHandle"]
        .as_str()
        .expect("workflow_start returns a handle")
        .to_owned();

    let response = call_tool(
        &live.server,
        81,
        "intent_complete_form",
        json!({
            "purpose":"fill the form",
            "fields":[{
                "name":"Email",
                "purpose":"Contact email",
                "value":{"kind":"setText","value":"someone@example.test"}
            }]
        }),
    )
    .await;
    assert!(
        response["error"].is_null(),
        "the call must be accepted and dispatched, never bounce off a schema rejection: {response}"
    );
    let outcome = &response["result"]["structuredContent"];
    assert_eq!(
        outcome["workflowId"], started["result"]["structuredContent"]["workflowId"],
        "{response}"
    );
    let defaulted = outcome["evidence"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|item| item["name"] == "workflowHandleDefaulted")
        .unwrap_or_else(|| panic!("workflowHandleDefaulted evidence missing: {response}"));
    assert_eq!(defaulted["kind"], "configuration", "{response}");
    assert_eq!(defaulted["value"], handle, "{response}");
}

/// Two live handles is not "the only one": defaulting must not guess between
/// them, and the resulting schema rejection names the live count so the
/// agent knows this is not the single-handle case it may have seen before.
#[tokio::test]
async fn scope_less_intent_complete_form_with_two_live_handles_is_rejected_naming_the_count() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    start(&live.server, 90, json!({"profile":"harness-a"})).await;
    start(&live.server, 91, json!({"profile":"harness-b"})).await;

    let response = call_tool(
        &live.server,
        92,
        "intent_complete_form",
        json!({
            "purpose":"fill the form",
            "fields":[{
                "name":"Email",
                "purpose":"Contact email",
                "value":{"kind":"setText","value":"someone@example.test"}
            }]
        }),
    )
    .await;
    assert_eq!(response["error"]["code"], -32602, "{response}");
    assert_eq!(
        response["error"]["data"]["reason"], "schemaViolation",
        "{response}"
    );
    assert_eq!(
        response["error"]["data"]["pointer"], "/sessionId",
        "{response}"
    );
    assert_eq!(
        response["error"]["data"]["constraint"], "required",
        "{response}"
    );
    let message = response["error"]["message"].as_str().unwrap();
    assert!(message.contains("2 live workflow handles"), "{message}");
    assert!(message.contains("workflow_start"), "{message}");
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

    // Unbounded capabilities, not an unknown protocol revision: a revision the gateway
    // does not speak is negotiated down to the newest one it does, so it is an accepted
    // re-initialize and would invalidate the handle.
    let rejected = live
        .server
        .handle_message(request(
            71,
            "initialize",
            json!({
                "protocolVersion":"2025-11-25",
                "capabilities":{"roots":"not-an-object"},
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

#[tokio::test]
async fn reusing_a_request_id_while_the_first_is_in_flight_returns_invalid_request_with_repair() {
    let handle = verified_handle(vec![
        Capability::SessionRead,
        Capability::SessionWrite,
        Capability::PageWrite,
    ])
    .await;
    let live = common::live_server_blocking_open(handle).await;
    initialize(&live.server).await;
    let server = Arc::clone(&live.server);
    let pending =
        tokio::spawn(async move { start(&server, 400, json!({"profile":"duplicate-id"})).await });

    tokio::time::timeout(
        StdDuration::from_secs(5),
        live.probe.open_entered.notified(),
    )
    .await
    .expect("open_page entered within five seconds");

    let duplicate = live
        .server
        .handle_message(request(400, "ping", json!({})))
        .await
        .expect("duplicate id response");
    assert_eq!(duplicate["error"]["code"], -32600, "{duplicate}");
    assert_eq!(
        duplicate["error"]["message"], "Invalid Request",
        "{duplicate}"
    );
    assert_eq!(
        duplicate["error"]["data"]["diagnostic"], "request id 400 is already in flight",
        "{duplicate}"
    );
    assert_eq!(
        duplicate["error"]["data"]["repair"],
        "use a unique id per request; wait for the earlier response or send notifications/cancelled for it first",
        "{duplicate}"
    );

    live.probe.open_release.notify_one();
    let response = tokio::time::timeout(StdDuration::from_secs(5), pending)
        .await
        .expect("original request completed")
        .unwrap();
    assert!(response.get("error").is_none(), "{response}");
}

fn workflow_handle_chrome_executable() -> std::path::PathBuf {
    std::env::var("BOBBY_CHROME_EXECUTABLE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        })
}

/// P2 end-to-end proof, live: the authorization journey driven entirely
/// through `workflowHandle` over the real MCP surface with an installed
/// browser. `click_and_wait_for_popup` follows the handle onto the popup;
/// the popup then closes itself (the way the authorization page does,
/// reproduced here the same way `runtime-tests`'
/// `popup_closed_from_inside_still_lists_the_opener` does it -- directly
/// against the runtime, since closing it is test setup, not the behavior
/// under test); the next handle-resolved call must land cleanly on the
/// opener with `popupClosed` evidence, with zero tool errors along the way.
#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn workflow_handle_follows_a_popup_and_returns_to_the_opener_when_it_closes() {
    let scenario = gauntlet_server::ScenarioServer::start(gauntlet_server::ScenarioConfig::seeded(
        "workflow-handle-popup-follow",
    ))
    .await
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let config = config::AppConfig {
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
        http: config::HttpConfig {
            allow_loopback: true,
            ..config::HttpConfig::default()
        },
        server: config::ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            shutdown_timeout_ms: 10_000,
        },
        browser: config::BrowserConfig {
            executable: Some(workflow_handle_chrome_executable()),
            profiles_dir: root.path().join("profiles"),
            headless: true,
            max_active: 8,
            upload_roots: vec![root.path().join("uploads")],
            downloads_dir: root.path().join("downloads"),
            artifacts_dir: root.path().join("artifacts"),
            max_artifact_bytes: 8 * 1024 * 1024,
            max_screenshot_dimension: 16_384,
            max_js_result_bytes: 64 * 1024,
            max_js_timeout_ms: 30_000,
        },
        storage: config::StorageConfig {
            journal_path: root.path().join("commands.jsonl"),
            checkpoints_dir: root.path().join("checkpoints"),
            authority_path: root.path().join("authority.json"),
            scheduler_journal_path: root.path().join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
        context: Default::default(),
        nodes: Default::default(),
    };
    let runtime_service = sdk_core::RuntimeService::build(&config).await.unwrap();
    // Kept aside, raw, only to close the popup from inside it below -- the
    // same live runtime the MCP server drives, not a second one.
    let raw_runtime = runtime_service.clone();

    let handle = verified_handle(vec![
        Capability::SessionRead,
        Capability::SessionWrite,
        Capability::PageWrite,
        Capability::BrowserMutate,
        Capability::RecoveryWrite,
        Capability::RecoveryRead,
    ])
    .await;
    let server = Server::new(Arc::new(sdk_core::AuthenticatedRuntime::new(
        runtime_service,
        handle,
    )));
    initialize(&server).await;

    let mut next_id = 900u64;
    async fn call(server: &Server, next_id: &mut u64, name: &str, arguments: Value) -> Value {
        *next_id += 1;
        call_tool(server, *next_id, name, arguments).await
    }

    let start = call(
        &server,
        &mut next_id,
        "workflow_start",
        json!({
            "profile": "workflow-handle-popup-follow",
            "url": scenario.application_url("/integrations"),
            // Only so the raw-runtime `window.close()` below (test setup,
            // not the behavior under test) is allowed to run.
            "executionPolicy": {"javascriptEvaluation": true},
        }),
    )
    .await;
    assert_eq!(
        start["result"]["structuredContent"]["status"], "completed",
        "{start}"
    );
    let workflow_handle = start["result"]["structuredContent"]["workflowHandle"]
        .as_str()
        .unwrap_or_else(|| panic!("workflow_start did not return a handle: {start}"))
        .to_owned();

    let followed = call(
        &server,
        &mut next_id,
        "click_and_wait_for_popup",
        json!({
            "workflowHandle": workflow_handle,
            "selector": "button[aria-label='Connect Ledger Cloud']",
            "target": null,
            "timeoutMs": 30_000,
        }),
    )
    .await;
    assert_eq!(
        followed["result"]["structuredContent"]["status"], "completed",
        "{followed}"
    );
    let popup_evidence = followed["result"]["structuredContent"]["evidence"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|item| item["kind"] == "popup")
        .unwrap_or_else(|| {
            panic!("click_and_wait_for_popup carried no popup evidence: {followed}")
        });
    let popup_page_id = popup_evidence["pageId"]
        .as_str()
        .expect("popup evidence names pageId")
        .to_owned();

    // Act in the popup via the handle: the handle is bound to it now.
    let in_popup = call(
        &server,
        &mut next_id,
        "a11y_snapshot",
        json!({"workflowHandle": workflow_handle, "maxNodes": 64}),
    )
    .await;
    assert_eq!(
        in_popup["result"]["structuredContent"]["status"], "completed",
        "{in_popup}"
    );

    // Close the popup from inside it, the way the authorization page does.
    // This is setup for the assertion below, not the behavior under test
    // (covered live by `popup_closed_from_inside_still_lists_the_opener` in
    // `runtime-tests`), so it goes straight to the runtime rather than
    // through the MCP `evaluate_javascript` tool and its own capability
    // gate.
    let popup_page_id: types::PageId = serde_json::from_value(json!(popup_page_id)).unwrap();
    let close = raw_runtime
        .submit(types::CommandEnvelope {
            schema_version: types::CommandEnvelope::SCHEMA_VERSION,
            command_id: types::CommandId::new(),
            workflow_id: types::WorkflowId::new(),
            attempt_id: types::AttemptId::new(),
            session_id: serde_json::from_value(
                start["result"]["structuredContent"]["sessionId"].clone(),
            )
            .unwrap(),
            page_id: Some(popup_page_id),
            deadline: Utc::now() + Duration::seconds(10),
            command: types::RuntimeCommand::Primitive(types::PrimitiveCommand::EvaluateJavaScript(
                types::EvaluateJavaScriptCommand {
                    expression: "window.close()".into(),
                    timeout_ms: 5_000,
                    await_promise: false,
                },
            )),
        })
        .await;
    // `window.close()` tears the target down out from under the eval's own
    // reply -- CDP can fail to read back a result from a context that is
    // already gone -- so this call's own outcome is unreliable by design;
    // only a hard policy/auth rejection (test setup gone wrong) fails loudly
    // here rather than downstream.
    assert!(
        !matches!(close, types::CommandOutcome::PolicyDenied { .. }),
        "{close:?}"
    );

    // The very next handle-resolved call must land cleanly on the opener.
    let observed = call(
        &server,
        &mut next_id,
        "workflow_observe",
        json!({"workflowHandle": workflow_handle}),
    )
    .await;
    assert_eq!(
        observed["result"]["structuredContent"]["status"], "completed",
        "{observed}"
    );
    let observed_evidence = observed["result"]["structuredContent"]["observationOutcome"]
        ["evidence"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        observed_evidence
            .iter()
            .any(|item| item["kind"] == "popupClosed"),
        "workflow_observe did not report popupClosed: {observed}"
    );
    assert!(
        observed_evidence
            .iter()
            .any(|item| item["kind"] == "accessibilitySnapshot"),
        "workflow_observe should still return live accessibility evidence from the opener: {observed}"
    );
}

/// Shared setup for the closed-page rule's fake-runtime tests (no Chrome
/// needed): a fresh workflow handle followed onto a popup through the fake
/// (`LiveWorker::click_and_wait_for_popup` fabricates the popup evidence;
/// the executor's own `register_page_id` and the server's `rebind_popup`
/// call are the same production code the real-Chrome test exercises), then
/// the fake told to fail every later command on the popup's page id with
/// the exact shape a real closed target returns
/// (`live.close_page_in_fake`).
async fn live_with_a_followed_popup_reported_closed() -> (
    common::LiveServer,
    String,
    types::SessionId,
    types::PageId,
    types::PageId,
) {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let started = start(&live.server, 1, json!({"profile": "popup-follow-fake"})).await;
    let result = &started["result"]["structuredContent"];
    assert_eq!(result["status"], "completed", "{started}");
    let workflow_handle = result["workflowHandle"]
        .as_str()
        .unwrap_or_else(|| panic!("workflow_start did not return a handle: {started}"))
        .to_owned();
    let session_id: types::SessionId = serde_json::from_value(result["sessionId"].clone())
        .expect("workflow_start returns a sessionId");
    let opener_page_id: types::PageId =
        serde_json::from_value(result["pageId"].clone()).expect("workflow_start returns a pageId");

    let followed = call_tool(
        &live.server,
        2,
        "click_and_wait_for_popup",
        json!({
            "workflowHandle": workflow_handle,
            "selector": "button",
            "target": null,
            "timeoutMs": 30_000,
        }),
    )
    .await;
    assert_eq!(
        followed["result"]["structuredContent"]["status"], "completed",
        "{followed}"
    );
    let popup_page_id: types::PageId = serde_json::from_value(
        followed["result"]["structuredContent"]["evidence"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|item| item["kind"] == "popup")
            .unwrap_or_else(|| {
                panic!("click_and_wait_for_popup carried no popup evidence: {followed}")
            })["pageId"]
            .clone(),
    )
    .expect("popup evidence names a pageId");

    live.close_page_in_fake(popup_page_id.clone());
    (
        live,
        workflow_handle,
        session_id,
        opener_page_id,
        popup_page_id,
    )
}

/// P5 gap 2 (read-only branch): a handle-resolved read-only call
/// (`a11y_snapshot`) on a popup the fake now reports closed replays once on
/// the recorded opener, succeeds, and carries `popupClosed` evidence naming
/// both pages. The handle is left resolving to the opener, so a second call
/// through it reaches the opener directly with no further `popupClosed`.
#[tokio::test]
async fn handle_resolved_read_only_call_replays_on_the_opener_once_the_popup_closes() {
    let (live, workflow_handle, _session_id, opener_page_id, popup_page_id) =
        live_with_a_followed_popup_reported_closed().await;

    let response = call_tool(
        &live.server,
        3,
        "a11y_snapshot",
        json!({"workflowHandle": workflow_handle, "maxNodes": 8}),
    )
    .await;
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["status"], "completed", "{response}");
    let popup_closed = content["evidence"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|item| item["kind"] == "popupClosed")
        .unwrap_or_else(|| panic!("a11y_snapshot did not report popupClosed: {response}"));
    assert_eq!(
        popup_closed["popupPageId"],
        json!(popup_page_id),
        "{response}"
    );
    assert_eq!(
        popup_closed["openerPageId"],
        json!(opener_page_id),
        "{response}"
    );

    let again = call_tool(
        &live.server,
        4,
        "a11y_snapshot",
        json!({"workflowHandle": workflow_handle, "maxNodes": 8}),
    )
    .await;
    let again_content = &again["result"]["structuredContent"];
    assert_eq!(again_content["status"], "completed", "{again}");
    assert!(
        again_content["evidence"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|item| item["kind"] != "popupClosed"),
        "the handle already resolves to the opener, so a second call needs no fallback: {again}"
    );
}

// NOT COMMITTED -- left in the working tree only. Reproduces a real
// production bug in `page-runtime`'s executor, outside this PR's four gaps:
// a closed-page failure with `page_state.is_some()` is classified
// `browser_died` on message text alone and revival's reattach branch
// (`reconnect_live_process` succeeds, since the browser process is alive --
// only the popup target closed) fires for every command class, but only
// `Replayable` commands (e.g. `a11y_snapshot`) retry transparently and
// surface a clean second failure afterward. `type_text` (not Replayable)
// takes the first failure as terminal, with its message suffixed
// "(CDP transport reset...)" and `retryable` forced `true`, landing on
// `CommandOutcome::RetryableFailure` -- `submit_envelope`'s closed-page rule
// only ever matches `status == "failed"`, so its mutating branch never
// fires for any non-Replayable command once a followed popup closes.
/// P5 gap 2 (mutating branch): a handle-resolved mutating call (`type_text`)
/// on a popup the fake now reports closed fails with `popupClosed` evidence
/// and a repair naming the opener -- never a silent retry, since that would
/// risk a second effect. The fake sees exactly one command. The handle is
/// still rebound to the opener afterward, so the next call through it
/// succeeds there directly.
#[tokio::test]
async fn handle_resolved_mutating_call_fails_with_popup_closed_evidence_and_repair() {
    let (live, workflow_handle, _session_id, opener_page_id, popup_page_id) =
        live_with_a_followed_popup_reported_closed().await;

    let before = live.type_text_calls();
    let response = call_tool(
        &live.server,
        3,
        "type_text",
        json!({"workflowHandle": workflow_handle, "selector": "input", "value": "hi"}),
    )
    .await;
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["status"], "failed", "{response}");
    let popup_closed = content["evidence"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|item| item["kind"] == "popupClosed")
        .unwrap_or_else(|| panic!("type_text did not report popupClosed: {response}"));
    assert_eq!(
        popup_closed["popupPageId"],
        json!(popup_page_id),
        "{response}"
    );
    assert_eq!(
        popup_closed["openerPageId"],
        json!(opener_page_id),
        "{response}"
    );
    let repair_action = content["error"]["repair"]["action"]
        .as_str()
        .unwrap_or_else(|| panic!("type_text failure carried no repair: {response}"));
    assert!(
        repair_action.contains(&opener_page_id.0.to_string()),
        "repair must name the opener page id: {repair_action}"
    );
    assert_eq!(
        live.type_text_calls(),
        before + 1,
        "the fake must see exactly one command -- no silent retry on a mutating call"
    );

    let again = call_tool(
        &live.server,
        4,
        "type_text",
        json!({"workflowHandle": workflow_handle, "selector": "input", "value": "again"}),
    )
    .await;
    assert_eq!(
        again["result"]["structuredContent"]["status"], "completed",
        "the handle must now resolve to the opener: {again}"
    );
    assert_eq!(live.type_text_calls(), before + 2);
}

/// P5 gap 2 (raw-id call): the same closed page, addressed by `sessionId`
/// + `pageId` instead of a handle, keeps the plain `notFound` failure and
/// carries no `popupClosed` evidence -- the closed-page rule only ever
/// touches a handle-resolved call.
#[tokio::test]
async fn raw_id_call_on_a_closed_page_keeps_the_generic_not_found_with_no_evidence() {
    let (live, _workflow_handle, session_id, _opener_page_id, popup_page_id) =
        live_with_a_followed_popup_reported_closed().await;

    let response = call_tool(
        &live.server,
        3,
        "a11y_snapshot",
        json!({"sessionId": session_id, "pageId": popup_page_id, "maxNodes": 8}),
    )
    .await;
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["status"], "failed", "{response}");
    assert_eq!(content["error"]["code"], "notFound", "{response}");
    assert!(
        content["evidence"]
            .as_array()
            .map(Vec::is_empty)
            .unwrap_or(true),
        "a raw-id call must never carry popupClosed evidence: {response}"
    );
}

/// P5 gap 1: `form_snapshot` bypasses `submit_envelope` (it calls
/// `self.runtime.form_snapshot` directly), so the closed-page rule needs its
/// own wiring for it. A handle-resolved call on the closed popup must
/// behave like the read-only branch above: succeed on the opener and carry
/// `popupClosed` evidence.
#[tokio::test]
async fn form_snapshot_through_the_handle_replays_on_the_opener_once_the_popup_closes() {
    let (live, workflow_handle, _session_id, opener_page_id, popup_page_id) =
        live_with_a_followed_popup_reported_closed().await;

    let response = call_tool(
        &live.server,
        3,
        "form_snapshot",
        json!({"workflowHandle": workflow_handle}),
    )
    .await;
    let content = &response["result"]["structuredContent"];
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(content["pageId"], json!(opener_page_id), "{response}");
    let popup_closed = content["evidence"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|item| item["kind"] == "popupClosed")
        .unwrap_or_else(|| panic!("form_snapshot did not report popupClosed: {response}"));
    assert_eq!(
        popup_closed["popupPageId"],
        json!(popup_page_id),
        "{response}"
    );
    assert_eq!(
        popup_closed["openerPageId"],
        json!(opener_page_id),
        "{response}"
    );
}

/// P5 gap 2 (`upload_files` controlId prerequisite): resolving a `controlId`
/// runs a `form_snapshot` lookup before the upload itself. On the popup the
/// fake now reports closed, that prerequisite lookup fails with
/// `popupClosed` evidence and a repair naming the opener -- never retried,
/// since it is a prerequisite of a mutating command -- and the fake never
/// sees an upload call (it has no working `upload_files` implementation, so
/// reaching one would fail a different way than asserted below). The handle
/// is left resolving to the opener, so a later call through it needs no
/// fallback.
#[tokio::test]
async fn upload_files_control_id_lookup_fails_with_popup_closed_evidence_and_repair() {
    let (live, workflow_handle, _session_id, opener_page_id, popup_page_id) =
        live_with_a_followed_popup_reported_closed().await;

    let response = call_tool(
        &live.server,
        3,
        "upload_files",
        json!({
            "workflowHandle": workflow_handle,
            "controlId": "file-input-1",
            "paths": ["/tmp/example.txt"],
        }),
    )
    .await;
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["status"], "failed", "{response}");
    assert_eq!(content["error"]["code"], "notFound", "{response}");
    let popup_closed = content["evidence"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|item| item["kind"] == "popupClosed")
        .unwrap_or_else(|| panic!("upload_files did not report popupClosed: {response}"));
    assert_eq!(
        popup_closed["popupPageId"],
        json!(popup_page_id),
        "{response}"
    );
    assert_eq!(
        popup_closed["openerPageId"],
        json!(opener_page_id),
        "{response}"
    );
    let repair_action = content["error"]["repair"]["action"]
        .as_str()
        .unwrap_or_else(|| panic!("upload_files failure carried no repair: {response}"));
    assert!(
        repair_action.contains(&opener_page_id.0.to_string()),
        "repair must name the opener page id: {repair_action}"
    );

    let again = call_tool(
        &live.server,
        4,
        "a11y_snapshot",
        json!({"workflowHandle": workflow_handle, "maxNodes": 8}),
    )
    .await;
    let again_content = &again["result"]["structuredContent"];
    assert_eq!(again_content["status"], "completed", "{again}");
    assert!(
        again_content["evidence"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|item| item["kind"] != "popupClosed"),
        "the handle already resolves to the opener, so a later call needs no fallback: {again}"
    );
}

/// The scope default covers `workflow_observe` too once it joined
/// `WORKFLOW_SCOPE_TOOLS`: a scope-less observe resolves against the one
/// live binding, and the outcome names the defaulted handle.
#[tokio::test]
async fn scope_less_workflow_observe_defaults_to_the_only_live_handle_and_reports_it() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let started = start(&live.server, 700, json!({"profile":"harness"})).await;
    let handle = started["result"]["structuredContent"]["workflowHandle"]
        .as_str()
        .expect("workflow_start returns a handle")
        .to_owned();

    let response = call_tool(&live.server, 701, "workflow_observe", json!({})).await;
    assert!(
        response["error"].is_null(),
        "the call must be accepted and dispatched, never bounce off a schema rejection: {response}"
    );
    let outcome = &response["result"]["structuredContent"];
    assert_eq!(
        outcome["workflowId"], started["result"]["structuredContent"]["workflowId"],
        "{response}"
    );
    assert_eq!(outcome["source"], "live", "{response}");
    let defaulted = outcome["evidence"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|item| item["name"] == "workflowHandleDefaulted")
        .unwrap_or_else(|| panic!("workflowHandleDefaulted evidence missing: {response}"));
    assert_eq!(defaulted["value"], handle, "{response}");
}

/// `page_activate {workflowHandle, pageId}` activates the named page and
/// rebinds the handle to it (same session), instead of refusing the mix —
/// the gauntlet agent left the handle path after two such conflicts.
#[tokio::test]
async fn page_activate_with_handle_and_page_id_activates_and_rebinds() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let started = start(&live.server, 710, json!({"profile":"harness"})).await;
    let start_outcome = &started["result"]["structuredContent"];
    let handle = start_outcome["workflowHandle"].as_str().unwrap();
    let page_id = start_outcome["pageId"].clone();

    // Open a second page through the same workflow handle.
    let opened = call_tool(
        &live.server,
        711,
        "page_open",
        json!({"sessionId": start_outcome["sessionId"], "url":"https://live-harness.test/second"}),
    )
    .await;
    assert!(opened["error"].is_null(), "{opened}");
    let second_page = opened["result"]["structuredContent"]["id"].clone();

    let activated = call_tool(
        &live.server,
        712,
        "page_activate",
        json!({"workflowHandle": handle, "pageId": second_page}),
    )
    .await;
    assert!(
        activated["error"].is_null(),
        "handle+pageId must activate and rebind, never a conflict: {activated}"
    );
    let outcome = &activated["result"]["structuredContent"];
    assert_eq!(outcome["status"], "completed", "{activated}");

    // The handle now observes the second page, not the original one.
    let observed = call_tool(
        &live.server,
        713,
        "workflow_observe",
        json!({"workflowHandle": handle}),
    )
    .await;
    assert_eq!(
        observed["result"]["structuredContent"]["pageId"], second_page,
        "the handle rebinds to the activated page: {observed}"
    );
    // The original page id is untouched for raw-id callers.
    assert_eq!(page_id, start_outcome["pageId"], "{page_id}");
}
