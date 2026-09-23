//! C2: on success, `intent_follow`, `intent_submit_and_verify`,
//! `intent_complete_form`, and boundary `click` carry `postState` -- the
//! same compact observation `workflow_observe` would return for the acting
//! page, built by the same function -- so a follow-up `workflow_observe`
//! call is redundant. Absent on a failed outcome.

mod common;

use std::sync::atomic::Ordering;

use chrono::{Duration, Utc};
use interface_core::AuthorityStore;
use mcp_gateway::Server;
use serde_json::{json, Value};
use types::{Capability, PrincipalId};
use uuid::uuid;

use common::{create_session_and_page, initialize, live_server, request, LiveServer};

async fn live_with_capabilities(capabilities: Vec<Capability>) -> LiveServer {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-0000000000c2")),
            capabilities,
            Utc::now() + Duration::hours(1),
        )
        .await
        .expect("issue capability token");
    let handle = authority
        .verify(&token.expose_once())
        .await
        .expect("verify token");
    let live = live_server(handle).await;
    initialize(&live.server).await;
    live
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

/// A resolvable clickable element, for `intent_follow`/`intent_submit_and_verify`'s
/// hints (`{"role":"button","accessibleName":"Continue"}`) to land on once a
/// test opts into it via `live.probe.candidates`.
fn clickable_candidate() -> dom_engine::Candidate {
    dom_engine::Candidate {
        id: "post-state-clickable".into(),
        css: Some("#continue".into()),
        tag: Some("button".into()),
        test_id: None,
        role: Some("button".into()),
        name: Some("Continue".into()),
        label: None,
        text: "Continue".into(),
        attributes: Default::default(),
        state: dom_engine::CandidateState {
            attached: true,
            visible: true,
            enabled: true,
        },
        frame_path: Vec::new(),
    }
}

/// A resolvable text field, for `intent_complete_form`'s field hints
/// (`{"role":"textbox","accessibleName":"Applicant email"}`).
fn textbox_candidate() -> dom_engine::Candidate {
    dom_engine::Candidate {
        id: "post-state-textbox".into(),
        css: Some("#email".into()),
        tag: Some("input".into()),
        test_id: None,
        role: Some("textbox".into()),
        name: Some("Applicant email".into()),
        label: None,
        text: String::new(),
        attributes: Default::default(),
        state: dom_engine::CandidateState {
            attached: true,
            visible: true,
            enabled: true,
        },
        frame_path: Vec::new(),
    }
}

/// `observationOutcome.commandId`/`attemptId` are minted fresh by every
/// `AccessibilitySnapshot` submission -- `postState`'s included -- so they
/// are normalized out before the equality check: the claim under test is
/// that `postState` carries the same compact *content* a fresh
/// `workflow_observe` would, not the same command identity.
fn without_call_identity(mut value: Value) -> Value {
    if let Some(object) = value
        .get_mut("observationOutcome")
        .and_then(Value::as_object_mut)
    {
        object.remove("commandId");
        object.remove("attemptId");
    }
    value
}

/// Asserts `response`'s `structuredContent.postState` is present and equals
/// (content-wise) a fresh `workflow_observe` call issued right after it,
/// against the same raw session/page/workflow ids.
async fn assert_post_state_matches_fresh_observe(
    live: &LiveServer,
    response: &Value,
    session_id: &str,
    page_id: &str,
    next_id: &mut u64,
) {
    let structured = &response["result"]["structuredContent"];
    let post_state = structured["postState"].clone();
    assert!(post_state.is_object(), "postState missing: {response}");
    let workflow_id = structured["workflowId"]
        .as_str()
        .expect("completed outcome names its workflowId")
        .to_owned();

    *next_id += 1;
    let observed = call_tool(
        &live.server,
        *next_id,
        "workflow_observe",
        json!({"sessionId":session_id,"pageId":page_id,"workflowId":workflow_id}),
    )
    .await;
    assert_eq!(
        without_call_identity(post_state),
        without_call_identity(observed["result"]["structuredContent"].clone()),
        "postState must equal a fresh workflow_observe for the same page: {response} vs {observed}"
    );
}

#[tokio::test]
async fn boundary_click_carries_post_state_on_success_and_omits_it_on_failure() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let mut next_id = 0u64;
    let (session, page) = create_session_and_page(&live.server, &mut next_id).await;
    let session_id = session.0.to_string();
    let page_id = page.0.to_string();

    // Failure: a Boundary click against a page that was never opened.
    let missing_page_id = uuid::Uuid::new_v4().to_string();
    next_id += 1;
    let failed = call_tool(
        &live.server,
        next_id,
        "click",
        json!({
            "sessionId":session_id,"pageId":missing_page_id,
            "selector":"#go","boundary":true,"autoCheckpoint":false
        }),
    )
    .await;
    assert_ne!(
        failed["result"]["structuredContent"]["status"], "completed",
        "{failed}"
    );
    assert!(
        failed["result"]["structuredContent"]
            .get("postState")
            .is_none(),
        "postState must be absent on failure: {failed}"
    );

    // Success: a selector-based click always lands against the fake worker.
    next_id += 1;
    let completed = call_tool(
        &live.server,
        next_id,
        "click",
        json!({
            "sessionId":session_id,"pageId":page_id,
            "selector":"#go","boundary":true,"autoCheckpoint":false
        }),
    )
    .await;
    assert_eq!(
        completed["result"]["structuredContent"]["status"], "completed",
        "{completed}"
    );
    assert_post_state_matches_fresh_observe(&live, &completed, &session_id, &page_id, &mut next_id)
        .await;
}

#[tokio::test]
async fn intent_follow_carries_post_state_on_success_and_omits_it_on_failure() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let mut next_id = 0u64;
    let (session, page) = create_session_and_page(&live.server, &mut next_id).await;
    let session_id = session.0.to_string();
    let page_id = page.0.to_string();
    // "contains" the empty string is trivially satisfied against any current
    // URL, including the fake worker's default (never navigated).
    let expected_destination = json!({
        "condition":{"kind":"url","matcher":{"kind":"contains","value":""}},
        "timeoutMs":2000
    });

    // Failure: no DOM candidate behind the fake, by default.
    next_id += 1;
    let failed = call_tool(
        &live.server,
        next_id,
        "intent_follow",
        json!({
            "sessionId":session_id,"pageId":page_id,
            "purpose":"open the continue link",
            "hints":{"role":"button","accessibleName":"Continue"},
            "expectedDestination":expected_destination,
            "autoCheckpoint":false
        }),
    )
    .await;
    assert_ne!(
        failed["result"]["structuredContent"]["status"], "completed",
        "{failed}"
    );
    assert!(
        failed["result"]["structuredContent"]
            .get("postState")
            .is_none(),
        "postState must be absent on failure: {failed}"
    );

    // Success: the fake DOM now hands back one matching candidate, and its
    // post-click verification wait is satisfied.
    live.probe
        .candidates
        .lock()
        .expect("candidates lock")
        .push(clickable_candidate());
    live.probe.satisfy_wait.store(true, Ordering::SeqCst);
    next_id += 1;
    let completed = call_tool(
        &live.server,
        next_id,
        "intent_follow",
        json!({
            "sessionId":session_id,"pageId":page_id,
            "purpose":"open the continue link",
            "hints":{"role":"button","accessibleName":"Continue"},
            "expectedDestination":expected_destination,
            "autoCheckpoint":false
        }),
    )
    .await;
    assert_eq!(
        completed["result"]["structuredContent"]["status"], "completed",
        "{completed}"
    );
    assert_post_state_matches_fresh_observe(&live, &completed, &session_id, &page_id, &mut next_id)
        .await;
}

#[tokio::test]
async fn intent_submit_and_verify_carries_post_state_on_success_and_omits_it_on_failure() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let mut next_id = 0u64;
    let (session, page) = create_session_and_page(&live.server, &mut next_id).await;
    let session_id = session.0.to_string();
    let page_id = page.0.to_string();
    // "contains" the empty string is trivially satisfied against any current
    // URL, including the fake worker's default (never navigated).
    let expected_state = json!({
        "condition":{"kind":"url","matcher":{"kind":"contains","value":""}},
        "timeoutMs":2000
    });

    // Failure: no DOM candidate behind the fake, by default.
    next_id += 1;
    let failed = call_tool(
        &live.server,
        next_id,
        "intent_submit_and_verify",
        json!({
            "sessionId":session_id,"pageId":page_id,
            "purpose":"submit the application",
            "hints":{"role":"button","accessibleName":"Continue"},
            "expectedState":expected_state,
            "autoCheckpoint":false
        }),
    )
    .await;
    assert_ne!(
        failed["result"]["structuredContent"]["status"], "completed",
        "{failed}"
    );
    assert!(
        failed["result"]["structuredContent"]
            .get("postState")
            .is_none(),
        "postState must be absent on failure: {failed}"
    );

    // Success: the fake DOM now hands back one matching candidate, and its
    // post-click verification wait is satisfied.
    live.probe
        .candidates
        .lock()
        .expect("candidates lock")
        .push(clickable_candidate());
    live.probe.satisfy_wait.store(true, Ordering::SeqCst);
    next_id += 1;
    let completed = call_tool(
        &live.server,
        next_id,
        "intent_submit_and_verify",
        json!({
            "sessionId":session_id,"pageId":page_id,
            "purpose":"submit the application",
            "hints":{"role":"button","accessibleName":"Continue"},
            "expectedState":expected_state,
            "autoCheckpoint":false,
            "reSubmit":true
        }),
    )
    .await;
    assert_eq!(
        completed["result"]["structuredContent"]["status"], "completed",
        "{completed}"
    );
    assert_post_state_matches_fresh_observe(&live, &completed, &session_id, &page_id, &mut next_id)
        .await;
}

#[tokio::test]
async fn intent_complete_form_carries_post_state_on_success_and_omits_it_on_failure() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let mut next_id = 0u64;
    let (session, page) = create_session_and_page(&live.server, &mut next_id).await;
    let session_id = session.0.to_string();
    let page_id = page.0.to_string();
    let fields = json!([{
        "name":"email",
        "purpose":"the applicant email",
        "hints":{"role":"textbox","accessibleName":"Applicant email"},
        "value":{"kind":"setText","value":"a@example.test","clearFirst":true}
    }]);

    // Failure: no DOM candidate behind the fake, by default.
    next_id += 1;
    let failed = call_tool(
        &live.server,
        next_id,
        "intent_complete_form",
        json!({
            "sessionId":session_id,"pageId":page_id,
            "purpose":"fill the application",
            "fields":fields
        }),
    )
    .await;
    assert_ne!(
        failed["result"]["structuredContent"]["status"], "completed",
        "{failed}"
    );
    assert!(
        failed["result"]["structuredContent"]
            .get("postState")
            .is_none(),
        "postState must be absent on failure: {failed}"
    );

    // Success: the fake DOM now hands back one matching candidate.
    live.probe
        .candidates
        .lock()
        .expect("candidates lock")
        .push(textbox_candidate());
    next_id += 1;
    let completed = call_tool(
        &live.server,
        next_id,
        "intent_complete_form",
        json!({
            "sessionId":session_id,"pageId":page_id,
            "purpose":"fill the application",
            "fields":fields
        }),
    )
    .await;
    assert_eq!(
        completed["result"]["structuredContent"]["status"], "completed",
        "{completed}"
    );
    assert_post_state_matches_fresh_observe(&live, &completed, &session_id, &page_id, &mut next_id)
        .await;
}
