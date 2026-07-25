//! F7: end-to-end acceptance for the JavaScript-evaluation feature's deny-by-default
//! invariant, driven over the real MCP JSON-RPC surface (`mcp_gateway::Server`) — the
//! same dispatch code (`schema::tool_schema`/`validate_tool_arguments`, `Server::call_tool`)
//! that `broker`'s `/v1/mcp` route wires up in production.
//!
//! Two independent gates must both pass before `EvaluateJavaScript` ever reaches a
//! worker:
//!   1. token capability gate (`javascript:evaluate`), enforced in
//!      `AuthenticatedRuntime::submit` before any session lookup.
//!   2. per-session `execution_policy.javascript_evaluation` gate, enforced in
//!      `RuntimeService::submit` before `PageRuntime::execute`.
//!
//! Gate A and Gate B are provable without a real browser: `RuntimeService::default()`
//! carries no worker pool, so `SessionManager::create` never leases (never launches
//! Chromium — see `crates/worker-pool/src/lib.rs::WorkerPool::lease`), and both gates
//! fire strictly before any worker dispatch. Only the happy path needs Chromium.

use std::sync::Arc;

use chrono::{Duration, Utc};
use interface_conformance::live::ChromeRuntimeHarness;
use interface_core::AuthorityStore;
use mcp_gateway::Server;
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use types::{Capability, PrincipalId};

/// Builds an in-process `mcp_gateway::Server` (no HTTP transport, no browser) backed by
/// `RuntimeService::default()`, authenticated as a fresh principal holding exactly
/// `capabilities`, and already through the MCP `initialize` handshake.
async fn fixture_server(capabilities: Vec<Capability>) -> Server {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            capabilities,
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let runtime = Arc::new(AuthenticatedRuntime::new(RuntimeService::default(), handle));
    let server = Server::new(runtime);
    initialize(&server).await;
    server
}

async fn initialize(server: &Server) {
    server
        .handle_message(request(
            1,
            "initialize",
            json!({
                "protocolVersion":"2025-11-25",
                "capabilities":{},
                "clientInfo":{"name":"javascript-evaluation-conformance","version":"1"}
            }),
        ))
        .await;
    server
        .handle_message(json!({
            "jsonrpc":"2.0","method":"notifications/initialized","params":{}
        }))
        .await;
}

fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}

async fn call_tool(server: &Server, id: u64, name: &str, arguments: Value) -> Value {
    server
        .handle_message(request(
            id,
            "tools/call",
            json!({"name":name,"arguments":arguments}),
        ))
        .await
        .unwrap()
}

/// Builds an `EvaluateJavaScript` `command_execute` tool-call payload the same way the
/// other MCP conformance/lifecycle tests build command envelopes: `schemaVersion`, fresh
/// ids, a near-future `deadline`, and `command: {kind, input}`.
fn evaluate_javascript_arguments(session_id: &str, page_id: Option<&str>) -> Value {
    json!({
        "envelope": {
            "schemaVersion": 2,
            "commandId": uuid::Uuid::new_v4(),
            "workflowId": uuid::Uuid::new_v4(),
            "attemptId": uuid::Uuid::new_v4(),
            "sessionId": session_id,
            "pageId": page_id,
            "deadline": (Utc::now() + Duration::seconds(30)).to_rfc3339(),
            "command": {
                "kind": "primitive",
                "input": {
                    "kind": "evaluateJavaScript",
                    "input": {
                        "expression": "6 * 7",
                        "timeoutMs": 5_000,
                        "awaitPromise": false
                    }
                }
            }
        },
        "idempotencyKey": format!("evaluate-javascript-{}", uuid::Uuid::new_v4())
    })
}

/// Gate A (capability): a token WITHOUT `javascript:evaluate` (only
/// session/page/browser:mutate) creates a JS-enabled session, then a `command_execute`
/// of `EvaluateJavaScript` must fail as a JSON-RPC/HTTP denial carrying
/// `MissingCapability` — before dispatch, no browser needed.
#[tokio::test]
async fn gate_a_missing_javascript_evaluate_capability_denies_before_dispatch() {
    let server = fixture_server(vec![
        Capability::SessionWrite,
        Capability::PageWrite,
        Capability::BrowserMutate,
    ])
    .await;

    let session = call_tool(
        &server,
        2,
        "session_create",
        json!({
            "profile": "gate-a",
            // The session itself is fully opted into JS: this isolates the assertion to
            // the capability gate, independent of the session policy gate (Gate B).
            "executionPolicy": {"javascriptEvaluation": true}
        }),
    )
    .await;
    assert_eq!(session["result"]["isError"], false, "{session}");
    let session_id = session["result"]["structuredContent"]["id"]
        .as_str()
        .expect("session_create returns an id")
        .to_owned();

    let denial = call_tool(
        &server,
        3,
        "command_execute",
        evaluate_javascript_arguments(&session_id, None),
    )
    .await;

    assert_eq!(
        denial["error"]["data"]["interfaceError"]["code"], "missingCapability",
        "{denial}"
    );
    assert_eq!(
        denial["error"]["data"]["interfaceError"]["requiredCapability"], "javascript:evaluate",
        "{denial}"
    );
}

/// Gate B (session policy): a token WITH `javascript:evaluate` creates a session
/// WITHOUT `executionPolicy` (deny-by-default), then a `command_execute` of
/// `EvaluateJavaScript` must produce a `PolicyDenied` outcome — before dispatch, no
/// browser needed.
#[tokio::test]
async fn gate_b_session_without_execution_policy_grant_is_policy_denied_before_dispatch() {
    let server = fixture_server(vec![
        Capability::SessionWrite,
        Capability::PageWrite,
        Capability::BrowserMutate,
        Capability::JavascriptEvaluate,
    ])
    .await;

    let session = call_tool(&server, 2, "session_create", json!({"profile": "gate-b"})).await;
    assert_eq!(session["result"]["isError"], false, "{session}");
    assert_eq!(
        session["result"]["structuredContent"]["execution_policy"]["javascriptEvaluation"], false,
        "session_create without executionPolicy must deny by default: {session}"
    );
    let session_id = session["result"]["structuredContent"]["id"]
        .as_str()
        .expect("session_create returns an id")
        .to_owned();

    let outcome = call_tool(
        &server,
        3,
        "command_execute",
        evaluate_javascript_arguments(&session_id, None),
    )
    .await;

    assert_eq!(outcome["result"]["isError"], false, "{outcome}");
    assert_eq!(
        outcome["result"]["structuredContent"]["status"], "policyDenied",
        "{outcome}"
    );
}

/// Happy path (needs a real Chrome/Chromium install): a token WITH
/// `javascript:evaluate`, a session WITH `executionPolicy.javascriptEvaluation = true`,
/// and an `EvaluateJavaScript` command actually runs and returns
/// `Evidence::JavaScriptResult { value: 42, .. }`.
#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn happy_path_evaluate_javascript_runs_on_real_chrome_and_returns_the_result() {
    let harness = ChromeRuntimeHarness::start().await;
    let runtime = Arc::new(AuthenticatedRuntime::new(
        harness.service.clone(),
        harness.handle.clone(),
    ));
    let server = Server::new(runtime);
    initialize(&server).await;

    let session = call_tool(
        &server,
        2,
        "session_create",
        json!({
            "profile": "happy-path",
            "executionPolicy": {"javascriptEvaluation": true}
        }),
    )
    .await;
    assert_eq!(session["result"]["isError"], false, "{session}");
    assert_eq!(
        session["result"]["structuredContent"]["execution_policy"]["javascriptEvaluation"], true,
        "{session}"
    );
    let session_id = session["result"]["structuredContent"]["id"]
        .as_str()
        .expect("session_create returns an id")
        .to_owned();

    let page = call_tool(&server, 3, "page_open", json!({"sessionId": session_id})).await;
    assert_eq!(page["result"]["isError"], false, "{page}");
    let page_id = page["result"]["structuredContent"]["id"]
        .as_str()
        .expect("page_open returns an id")
        .to_owned();

    let outcome = call_tool(
        &server,
        4,
        "command_execute",
        evaluate_javascript_arguments(&session_id, Some(&page_id)),
    )
    .await;

    assert_eq!(outcome["result"]["isError"], false, "{outcome}");
    assert_eq!(
        outcome["result"]["structuredContent"]["status"], "completed",
        "{outcome}"
    );
    let evidence = outcome["result"]["structuredContent"]["evidence"]
        .as_array()
        .expect("completed outcome carries evidence");
    let js_result = evidence
        .iter()
        .find(|item| item["kind"] == "javaScriptResult")
        .unwrap_or_else(|| panic!("expected a javaScriptResult evidence item: {outcome}"));
    assert_eq!(js_result["value"], json!(42), "{outcome}");
    assert_eq!(js_result["truncated"], false, "{outcome}");
}
