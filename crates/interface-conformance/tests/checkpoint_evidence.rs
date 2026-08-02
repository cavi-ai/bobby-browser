//! Evidence authorship is a runtime property, not a per-adapter one.
//!
//! `checkpoint_save` over MCP was changed to take `evidenceRefs` — command ids
//! the runtime resolves against its own journal — precisely so a caller cannot
//! author evidence for work it never performed. That property is worthless if
//! another adapter still accepts `Evidence` straight off the wire, and the
//! existing conformance suites (capability, idempotency, event ordering) do not
//! cover it: they assert that adapters agree on *outcomes*, not that they agree
//! on *who is trusted to describe them*.
//!
//! This file asserts the property directly, on every adapter that exposes
//! checkpointing to a principal.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use broker::testing::{app_with_admin, context_headers, issue_bearer};
use chrono::{Duration, Utc};
use interface_core::AuthorityStore;
use mcp_gateway::Server;
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use tower::ServiceExt;
use types::{
    AttemptId, Capability, CheckpointId, CommandClass, CommandId, Evidence, PageId, PrincipalId,
    SessionId, WorkflowCheckpoint, WorkflowId,
};
use uuid::uuid;

const PRINCIPAL: uuid::Uuid = uuid!("00000000-0000-0000-0000-0000000000e1");

/// The set `broker::testing`'s bootstrap admin can delegate: `authority:admin`
/// is never issuable to a sub-principal, and issuance rejects anything the
/// issuer does not itself hold.
const DELEGABLE_CAPABILITY_NAME: [&str; 7] = [
    "session:read",
    "session:write",
    "page:read",
    "page:write",
    "browser:mutate",
    "recovery:read",
    "recovery:write",
];

const EVERY_CAPABILITY: [Capability; 15] = [
    Capability::SessionRead,
    Capability::SessionWrite,
    Capability::PageRead,
    Capability::PageWrite,
    Capability::BrowserMutate,
    Capability::FileUpload,
    Capability::FileDownload,
    Capability::JavascriptEvaluate,
    Capability::IntentExecute,
    Capability::VisionAssist,
    Capability::ArtifactRead,
    Capability::ArtifactCapture,
    Capability::RecoveryRead,
    Capability::RecoveryWrite,
    Capability::AuthorityAdmin,
];

/// A checkpoint that is structurally valid, so a rejection can only be about
/// the evidence argument and never about the checkpoint body.
fn checkpoint() -> WorkflowCheckpoint {
    WorkflowCheckpoint {
        schema_version: 1,
        checkpoint_id: CheckpointId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: PageId::new(),
        restart_url: "https://example.test/".to_owned(),
        current_url: "https://example.test/".to_owned(),
        cursor: Some(CommandId::new()),
        boundary_command_id: None,
        recovery_class: CommandClass::Boundary,
        invariants: Vec::new(),
        replayable_inputs: Vec::new(),
        evidence: Vec::new(),
        recovery_history: Vec::new(),
        recovery_receipts: Vec::new(),
        created_at: Utc::now(),
    }
}

/// Evidence a caller could plausibly fabricate. This must deserialize cleanly
/// as a real `types::Evidence` — otherwise the surface rejects it for being
/// malformed and the test proves nothing about who is trusted to author it.
fn fabricated_evidence() -> Value {
    let evidence = vec![Evidence::Navigation {
        url: "https://example.test/thank-you".to_owned(),
        title: "Thank you".to_owned(),
    }];
    serde_json::to_value(&evidence).expect("evidence serializes")
}

/// Guards the guard: if `Evidence`'s wire shape changes and the fixture stops
/// round-tripping, every rejection below would pass for the wrong reason.
#[test]
fn the_fabricated_evidence_fixture_is_a_valid_evidence_payload() {
    let parsed: Vec<Evidence> =
        serde_json::from_value(fabricated_evidence()).expect("fixture is real Evidence");
    assert_eq!(parsed.len(), 1);
}

async fn mcp_server() -> Server {
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            EVERY_CAPABILITY,
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
                      "clientInfo":{"name":"checkpoint-evidence","version":"1"}}
        }))
        .await
        .expect("initialize answers");
    server
        .handle_message(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .await;
    server
}

#[tokio::test]
async fn mcp_refuses_caller_authored_checkpoint_evidence() {
    let server = mcp_server().await;
    let response = server
        .handle_message(json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"checkpoint_save","arguments":{
                "checkpoint": checkpoint(),
                "evidence": fabricated_evidence()
            }}
        }))
        .await
        .expect("tools/call answers");
    assert!(
        response.get("error").is_some(),
        "MCP accepted caller-authored evidence: {response}"
    );
}

/// POSTs a checkpoint body and returns `(status, error code)`.
async fn post_checkpoint(body: Value) -> (StatusCode, String) {
    let (app, _authority, admin) = app_with_admin(4).await;
    let bearer = issue_bearer(&app, &admin, PRINCIPAL, &DELEGABLE_CAPABILITY_NAME).await;
    let request = context_headers(Request::post("/v1/checkpoints"), &bearer)
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
    let code = serde_json::from_slice::<Value>(&payload)
        .ok()
        .and_then(|body| body["error"]["code"].as_str().map(str::to_owned))
        .unwrap_or_default();
    (status, code)
}

/// Asserting on the error *code*, not merely on non-200, is what makes this
/// bite. `RuntimeService` in this fixture has no recovery coordinator, so a
/// checkpoint that reaches the runtime fails either way — with `notFound`.
/// Only a body rejected at the boundary produces `invalidRequest`, and that is
/// exactly the claim: `evidence` is not an argument this surface accepts.
#[tokio::test]
async fn http_refuses_caller_authored_checkpoint_evidence() {
    let (status, code) =
        post_checkpoint(json!({"checkpoint": checkpoint(), "evidence": fabricated_evidence()}))
            .await;
    assert_eq!(
        (status, code.as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalidRequest"),
        "HTTP still accepts a caller-supplied evidence argument"
    );
}

/// The other half of the same claim: `evidenceRefs` *is* accepted, so the test
/// above cannot pass by the surface rejecting every checkpoint body.
#[tokio::test]
async fn http_accepts_evidence_refs() {
    let (status, code) =
        post_checkpoint(json!({"checkpoint": checkpoint(), "evidenceRefs": [CommandId::new()]}))
            .await;
    assert_ne!(
        (status, code.as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalidRequest"),
        "HTTP rejected evidenceRefs as an unknown argument"
    );
}

/// The adapters must not merely both reject — they must reject for the same
/// reason, which is that the argument does not exist on either surface. A
/// surface that accepted the key and silently dropped it would pass a
/// "rejects fabricated evidence" test while still lying to its caller.
#[tokio::test]
async fn no_adapter_advertises_a_caller_supplied_evidence_argument() {
    let server = mcp_server().await;
    let listed = server
        .handle_message(json!({"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}))
        .await
        .expect("tools/list answers");
    let tool = listed["result"]["tools"]
        .as_array()
        .expect("tools is an array")
        .iter()
        .find(|tool| tool["name"] == "checkpoint_save")
        .expect("checkpoint_save is advertised")
        .clone();
    let properties = &tool["inputSchema"]["properties"];
    assert!(
        properties["evidenceRefs"].is_object(),
        "checkpoint_save must advertise evidenceRefs: {tool}"
    );
    assert!(
        properties.get("evidence").is_none(),
        "checkpoint_save still advertises a caller-supplied evidence argument: {tool}"
    );
}

/// `evidenceRefs` naming a command with no journal record resolves to nothing
/// and fails the checkpoint, rather than persisting one with empty evidence.
/// Both adapters must agree on that too.
#[tokio::test]
async fn both_adapters_reject_evidence_refs_with_no_journal_record() {
    let unrecorded = CommandId::new();

    let server = mcp_server().await;
    let mcp = server
        .handle_message(json!({
            "jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"checkpoint_save","arguments":{
                "checkpoint": checkpoint(),
                "evidenceRefs": [unrecorded]
            }}
        }))
        .await
        .expect("tools/call answers");
    let mcp_failed = mcp.get("error").is_some() || mcp["result"]["isError"] == json!(true);
    assert!(mcp_failed, "MCP accepted an unrecorded command id: {mcp}");

    let (status, _) =
        post_checkpoint(json!({"checkpoint": checkpoint(), "evidenceRefs": [unrecorded]})).await;
    assert_ne!(
        status,
        StatusCode::OK,
        "HTTP accepted an unrecorded command id"
    );
}
