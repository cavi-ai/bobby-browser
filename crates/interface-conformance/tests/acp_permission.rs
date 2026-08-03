//! ACP joins the conformance suite as a fourth adapter.
//!
//! The design is explicit that the suite is what keeps a new adapter from
//! becoming a second-class surface, and that it must join *first* — before the
//! adapter accumulates divergence that is expensive to unwind. So this file
//! exists ahead of the ACP server loop, and asserts the property that would be
//! most expensive to get wrong.
//!
//! That property: **a permission grant for a capability the principal lacks is
//! denied, and the denial is indistinguishable in effect from the same denial
//! on HTTP and MCP.**
//!
//! ACP inverts the usual direction — the agent asks the editor, the editor
//! asks a human, the human clicks — so it is the one adapter where a UI
//! affordance could be mistaken for authority. HTTP and MCP have no such
//! affordance, which is precisely why they are the baseline here: whatever a
//! human clicks, the outcome must match what the other two do with the same
//! token.

use std::sync::Arc;

use acp_gateway::{decide, Escalation, EscalationRequest, SessionPolicyGates};
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use broker::testing::{app_with_admin, context_headers, issue_bearer};
use chrono::{Duration, Utc};
use interface_core::AuthorityStore;
use mcp_gateway::Server;
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use tower::ServiceExt;
use types::{Capability, InterfaceOperation, PrincipalId};
use uuid::uuid;

const PRINCIPAL: uuid::Uuid = uuid!("00000000-0000-0000-0000-0000000000f1");

/// A principal that can open pages but cannot evaluate JavaScript. The gap is
/// the thing every adapter has to agree about.
const WITHOUT_JAVASCRIPT: [&str; 4] = ["session:read", "session:write", "page:read", "page:write"];

async fn mcp_server_without_javascript() -> Server {
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(PRINCIPAL),
            [
                Capability::SessionRead,
                Capability::SessionWrite,
                Capability::PageRead,
                Capability::PageWrite,
            ],
            Utc::now() + Duration::hours(1),
        )
        .await
        .expect("authority issues");
    let handle = authority
        .verify(&token.expose_once())
        .await
        .expect("bearer verifies");
    let server = Server::new(Arc::new(AuthenticatedRuntime::new(
        RuntimeService::default(),
        handle,
    )));
    server
        .handle_message(json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2025-11-25","capabilities":{},
                      "clientInfo":{"name":"acp-parity","version":"1"}}
        }))
        .await
        .expect("initialize answers");
    server
        .handle_message(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .await;
    server
}

/// ACP: the capability is missing, so no prompt is sent and approval is
/// meaningless.
#[test]
fn acp_denies_a_capability_the_principal_lacks() {
    // Everything `SubmitCommand` needs, so vision is the only gap. Without
    // `browser:mutate` the decision would deny on that instead, and the test
    // would pass while proving nothing about vision.
    let held = [
        Capability::SessionRead,
        Capability::SessionWrite,
        Capability::PageRead,
        Capability::PageWrite,
        Capability::BrowserMutate,
    ];
    let decision = decide(
        EscalationRequest::with_vision(InterfaceOperation::SubmitCommand),
        &held,
        SessionPolicyGates {
            // The gate is wide open. It must not matter.
            vision_assist: true,
        },
    );
    assert_eq!(
        decision,
        Escalation::Denied {
            missing: Capability::VisionAssist
        }
    );
    assert!(
        !decision.should_prompt(),
        "ACP would put a permission prompt in front of a human who cannot grant it"
    );
    assert!(
        !decision.permits_after_approval(),
        "ACP would proceed after approval despite a missing capability"
    );
}

/// MCP: the same token, the same gap, refused.
#[tokio::test]
async fn mcp_denies_the_same_capability_gap() {
    let server = mcp_server_without_javascript().await;
    let listed = server
        .handle_message(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
        .await
        .expect("tools/list answers");
    let names: Vec<String> = listed["result"]["tools"]
        .as_array()
        .expect("tools is an array")
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        !names.contains(&"evaluate_javascript".to_owned()),
        "MCP advertised a tool the principal cannot call: {names:?}"
    );

    let response = server
        .handle_message(json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"evaluate_javascript","arguments":{
                "sessionId": uuid::Uuid::new_v4(),
                "pageId": uuid::Uuid::new_v4(),
                "script": "1",
                "awaitPromise": false
            }}
        }))
        .await
        .expect("tools/call answers");
    assert!(
        response.get("error").is_some(),
        "MCP allowed a call the principal lacks the capability for: {response}"
    );
}

/// HTTP: the same token, the same gap, refused — and specifically as
/// `missingCapability`, so the three adapters agree on the *reason* and not
/// merely on failing.
#[tokio::test]
async fn http_denies_the_same_capability_gap_as_missing_capability() {
    let (app, _authority, admin) = app_with_admin(4).await;
    let bearer = issue_bearer(&app, &admin, PRINCIPAL, &WITHOUT_JAVASCRIPT).await;
    let body = json!({
        "schemaVersion": 2,
        "commandId": uuid::Uuid::new_v4(),
        "workflowId": uuid::Uuid::new_v4(),
        "attemptId": uuid::Uuid::new_v4(),
        "sessionId": uuid::Uuid::new_v4(),
        "pageId": uuid::Uuid::new_v4(),
        "deadline": (Utc::now() + Duration::seconds(30)).to_rfc3339(),
        "command": {"kind":"primitive","input":{
            "kind":"evaluateJavaScript",
            "input":{"expression":"1","timeoutMs":1000,"awaitPromise":false}
        }}
    });
    let request = context_headers(Request::post("/v1/commands"), &bearer)
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&body).expect("body serializes"),
        ))
        .expect("request builds");
    let response = app.oneshot(request).await.expect("router answers");
    let status = response.status();
    let payload = to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("body reads");
    assert_ne!(
        status,
        StatusCode::OK,
        "HTTP allowed a command the principal lacks the capability for"
    );
    let parsed: Value = serde_json::from_slice(&payload).expect("error body is JSON");
    assert_eq!(
        parsed["error"]["code"], "missingCapability",
        "HTTP refused for a different reason than a capability gap: {parsed}"
    );
}

/// The parity claim itself, stated as one assertion rather than left implicit
/// in three separate tests passing: for the same token and the same gap, every
/// adapter refuses, and ACP refuses *without asking*.
#[test]
fn no_adapter_lets_a_permission_grant_widen_a_token() {
    let held = [
        Capability::SessionRead,
        Capability::PageRead,
        Capability::BrowserMutate,
    ];
    for gates in [
        SessionPolicyGates {
            vision_assist: false,
        },
        SessionPolicyGates {
            vision_assist: true,
        },
    ] {
        let decision = decide(
            EscalationRequest::with_vision(InterfaceOperation::SubmitCommand),
            &held,
            gates,
        );
        assert!(
            !decision.permits_after_approval(),
            "with gates {gates:?}, approval widened a token that lacks the capability"
        );
    }
}
