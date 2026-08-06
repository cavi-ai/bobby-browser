//! Evidence authorship is a runtime property, not a per-adapter one.
//!
//! Checkpointing takes `evidenceRefs`, command ids the runtime resolves against
//! its own journal, so a caller cannot author evidence for work it never
//! performed. Asserted here on every adapter that exposes checkpointing to a
//! principal.

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

/// Evidence a caller could plausibly fabricate. Must deserialize as a real
/// `types::Evidence`, or a rejection only proves the payload was malformed.
fn fabricated_evidence() -> Value {
    let evidence = vec![Evidence::Navigation {
        url: "https://example.test/thank-you".to_owned(),
        title: "Thank you".to_owned(),
    }];
    serde_json::to_value(&evidence).expect("evidence serializes")
}

/// If `Evidence`'s wire shape changes and the fixture stops round-tripping,
/// every rejection below passes for the wrong reason.
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

/// Asserts the error *code*, not merely non-200: this fixture's
/// `RuntimeService` has no recovery coordinator, so a checkpoint that reaches
/// the runtime fails with `notFound`. Only a body rejected at the boundary
/// produces `invalidRequest`.
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

/// `evidenceRefs` is accepted, so the test above cannot pass by the surface
/// rejecting every checkpoint body.
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

/// Both adapters must reject because the argument does not exist on either
/// surface, not by accepting the key and silently dropping it.
#[tokio::test]
async fn no_adapter_advertises_a_caller_supplied_evidence_argument() {
    let server = mcp_server().await;
    // `tools/list` defaults to the narrow explore phase, which advertises
    // neither of these tools. This asserts a schema's shape, not the default
    // phase's membership, so widen first.
    server
        .handle_message(json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"toolset_select","arguments":{"toolset":"full"}}
        }))
        .await
        .expect("toolset_select answers");
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

/// `evidenceRefs` naming a command with no journal record fails the checkpoint
/// on both adapters, rather than persisting one with empty evidence.
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
