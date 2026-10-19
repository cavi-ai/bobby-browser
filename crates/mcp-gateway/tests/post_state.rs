//! On success, `intent_follow`, `intent_submit_and_verify`,
//! `intent_complete_form`, opted-in `click`/`navigate`, and boundary clicks by default carry
//! `postState`, the compact
//! observation `workflow_observe` returns for the acting page. Absent on a
//! failed outcome.

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

#[tokio::test]
async fn plain_click_state_is_opt_in_without_recovery_authority() {
    let live = live_with_capabilities(vec![
        Capability::SessionWrite,
        Capability::PageWrite,
        Capability::BrowserMutate,
    ])
    .await;
    let mut next_id = 100;
    let (session, page) = create_session_and_page(&live.server, &mut next_id).await;
    for (index, flag) in [None, Some(false), Some(true)].into_iter().enumerate() {
        let mut args = json!({"sessionId":session,"pageId":page,"selector":"#continue"});
        if let Some(flag) = flag {
            args["includePostState"] = json!(flag);
        }
        let before = live.accessibility_calls();
        let response = call_tool(&live.server, 110 + index as u64, "click", args).await;
        let result = &response["result"]["structuredContent"];
        assert_eq!(result["status"], "completed", "{response}");
        assert_eq!(
            live.accessibility_calls() - before,
            usize::from(flag == Some(true))
        );
        if flag == Some(true) {
            assert_post_state_matches_fresh_observe(
                &live,
                &response,
                &session.0.to_string(),
                &page.0.to_string(),
                &mut next_id,
            )
            .await;
        } else {
            assert!(result.get("postState").is_none());
            assert!(result.get("postStateStatus").is_none());
        }
    }
}

#[tokio::test]
async fn boundary_click_can_skip_state_without_skipping_its_checkpoint() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let (session, page) = create_session_and_page(&live.server, &mut 120).await;
    let response = call_tool(
        &live.server,
        130,
        "click",
        json!({
            "sessionId":session,"pageId":page,"selector":"#continue",
            "boundary":true,"includePostState":false
        }),
    )
    .await;
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["status"], "completed", "{response}");
    assert!(result.get("postState").is_none(), "{response}");
    assert!(result.get("postStateStatus").is_none(), "{response}");
    assert_eq!(live.accessibility_calls(), 0);
    assert!(result["checkpointId"].is_string(), "{response}");
}

#[tokio::test]
async fn plain_click_read_failure_keeps_the_effect_and_invalid_requests_do_not_dispatch() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let (session, page) = create_session_and_page(&live.server, &mut 140).await;
    let invalid = call_tool(
        &live.server,
        150,
        "click",
        json!({
            "sessionId":session,"pageId":page,"selector":"#continue","includePostState":"yes"
        }),
    )
    .await;
    assert!(invalid.get("error").is_some(), "{invalid}");
    assert_eq!(live.probe.click_calls.load(Ordering::SeqCst), 0);
    assert_eq!(live.accessibility_calls(), 0);

    live.probe
        .accessibility_failures_remaining
        .store(20, Ordering::SeqCst);
    let clicked = call_tool(
        &live.server,
        151,
        "click",
        json!({
            "sessionId":session,"pageId":page,"selector":"#continue","includePostState":true
        }),
    )
    .await;
    let result = &clicked["result"]["structuredContent"];
    assert_eq!(result["status"], "completed", "{clicked}");
    assert_eq!(clicked["result"]["isError"], false, "{clicked}");
    assert_eq!(live.probe.click_calls.load(Ordering::SeqCst), 1);
    assert!(result.get("postState").is_none());
    assert_eq!(
        result["postStateStatus"],
        json!({"status":"unavailable","code":"observationIncomplete","reconciliationRequired":false})
    );
    let diagnostic = result["postStateStatus"].to_string();
    assert!(diagnostic.len() <= 256 && !diagnostic.contains("injected"));

    let reads = live.accessibility_calls();
    let failed = call_tool(&live.server, 152, "click", json!({
        "sessionId":session,"pageId":uuid::Uuid::new_v4(),"selector":"#continue","includePostState":true
    })).await;
    let result = &failed["result"]["structuredContent"];
    assert_ne!(result["status"], "completed", "{failed}");
    assert!(result.get("postState").is_none());
    assert_eq!(result["postStateStatus"]["status"], "notRequested");
    assert_eq!(live.accessibility_calls(), reads);
    assert_eq!(live.probe.click_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn navigate_returns_optional_state_and_preserves_navigation_on_read_failure() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let mut next_id = 40;
    let (session, page) = create_session_and_page(&live.server, &mut next_id).await;
    let completed = call_tool(
        &live.server,
        50,
        "navigate",
        json!({"sessionId":session,"pageId":page,"url":"https://live-harness.test/","includePostState":true}),
    )
    .await;
    assert_eq!(
        completed["result"]["structuredContent"]["status"], "completed",
        "{completed}"
    );
    assert_eq!(live.accessibility_calls(), 1);
    assert_post_state_matches_fresh_observe(
        &live,
        &completed,
        &session.0.to_string(),
        &page.0.to_string(),
        &mut next_id,
    )
    .await;
    let state = &completed["result"]["structuredContent"]["postState"];
    assert_eq!(state["sessionId"], json!(session));
    assert_eq!(state["pageId"], json!(page));
    assert_eq!(state["pageDerived"], true);
    assert_eq!(
        live.probe
            .last_accessibility_max_nodes
            .load(Ordering::SeqCst),
        256
    );

    live.probe
        .accessibility_failures_remaining
        .store(20, Ordering::SeqCst);
    let unavailable = call_tool(
        &live.server,
        51,
        "navigate",
        json!({"sessionId":session,"pageId":page,"url":"https://live-harness.test/next","includePostState":true}),
    ).await;
    let result = &unavailable["result"]["structuredContent"];
    assert_eq!(result["status"], "completed", "{unavailable}");
    assert_eq!(unavailable["result"]["isError"], false, "{unavailable}");
    assert!(result.get("postState").is_none());
    assert_eq!(
        result["postStateStatus"],
        json!({"status":"unavailable","code":"observationIncomplete","reconciliationRequired":false})
    );
    assert!(!result
        .to_string()
        .contains("injected live-harness accessibility failure"));
}

#[tokio::test]
async fn navigate_without_state_or_with_failed_navigation_never_observes() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let (session, page) = create_session_and_page(&live.server, &mut 60).await;
    for (index, flag) in [json!({}), json!({"includePostState":false})]
        .into_iter()
        .enumerate()
    {
        let mut args =
            json!({"sessionId":session,"pageId":page,"url":"https://live-harness.test/"});
        args.as_object_mut()
            .unwrap()
            .extend(flag.as_object().unwrap().clone());
        let response = call_tool(&live.server, 70 + index as u64, "navigate", args).await;
        let result = &response["result"]["structuredContent"];
        assert_eq!(result["status"], "completed", "{response}");
        assert!(result.get("postState").is_none());
        assert!(result.get("postStateStatus").is_none());
    }
    let failed = call_tool(&live.server, 72, "navigate", json!({"sessionId":session,"pageId":page,"url":"https://live-harness.test/fail","includePostState":true})).await;
    let result = &failed["result"]["structuredContent"];
    assert_ne!(result["status"], "completed", "{failed}");
    assert!(result.get("postState").is_none());
    assert_eq!(result["postStateStatus"]["status"], "notRequested");
    assert_eq!(live.accessibility_calls(), 0);
}

#[tokio::test]
async fn navigate_state_uses_the_retained_binding_and_rejects_invalid_requests_before_dispatch() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let started = call_tool(
        &live.server,
        80,
        "workflow_start",
        json!({"profile":"navigation-state"}),
    )
    .await;
    let binding = &started["result"]["structuredContent"];
    let navigated = call_tool(
        &live.server,
        81,
        "navigate",
        json!({
            "workflowHandle":binding["workflowHandle"],
            "url":"https://live-harness.test/bound", "includePostState":true
        }),
    )
    .await;
    let result = &navigated["result"]["structuredContent"];
    assert_eq!(result["status"], "completed", "{navigated}");
    assert_eq!(
        result["postStateStatus"]["status"], "available",
        "{navigated}"
    );
    for field in ["sessionId", "pageId", "workflowHandle"] {
        assert_eq!(
            result["postState"][field], binding[field],
            "{field}: {navigated}"
        );
    }
    assert!(result["workflowId"].is_string());
    assert_eq!(result["postState"]["workflowId"], result["workflowId"]);

    for (index, args) in [
        json!({"workflowHandle":binding["workflowHandle"],"url":"https://live-harness.test/invalid","includePostState":"yes"}),
        json!({"workflowHandle":"wh_unknown","url":"https://live-harness.test/invalid","includePostState":true}),
    ].into_iter().enumerate() {
        let denied = call_tool(&live.server, 82 + index as u64, "navigate", args).await;
        assert!(denied.get("error").is_some(), "{denied}");
    }
    assert_eq!(live.accessibility_calls(), 1);
    assert_eq!(
        live.probe.current_url.lock().unwrap().as_deref(),
        Some("https://live-harness.test/bound")
    );
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
    assert_eq!(structured["postStateStatus"]["status"], "available");
    assert!(
        serde_json::to_vec(&structured["postStateStatus"])
            .unwrap()
            .len()
            <= 256
    );
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
    assert_eq!(
        completed["result"]["structuredContent"]["verificationStatus"],
        "notReported"
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
    assert_eq!(
        failed["result"]["structuredContent"]["verificationStatus"],
        "notCompleted"
    );
    assert_eq!(
        completed["result"]["structuredContent"]["verificationStatus"],
        "conditionObserved"
    );
    assert_post_state_matches_fresh_observe(&live, &completed, &session_id, &page_id, &mut next_id)
        .await;
}

#[tokio::test]
async fn post_state_is_observed_when_the_action_carries_an_idempotency_key() {
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let mut next_id = 0u64;
    let (session, page) = create_session_and_page(&live.server, &mut next_id).await;
    let session_id = session.0.to_string();
    let page_id = page.0.to_string();
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
            "expectedState":{
                "condition":{"kind":"url","matcher":{"kind":"contains","value":""}},
                "timeoutMs":2000
            },
            "autoCheckpoint":false,
            "idempotencyKey":"submit-application-1"
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

#[tokio::test]
async fn optional_observation_failure_preserves_completed_action_and_reports_bounded_status() {
    use std::sync::atomic::Ordering;
    let live = live_with_capabilities(Capability::ALL.to_vec()).await;
    let (session_id, page_id) = create_session_and_page(&live.server, &mut 900).await;
    live.probe
        .candidates
        .lock()
        .unwrap()
        .push(clickable_candidate());
    live.probe
        .accessibility_failures_remaining
        .store(20, Ordering::SeqCst);
    let response = call_tool(
        &live.server,
        910,
        "click",
        json!({"sessionId":session_id,"pageId":page_id,"selector":"#continue","boundary":true,"autoCheckpoint":false}),
    )
    .await;
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["status"], "completed", "{response}");
    assert_eq!(content["verificationStatus"], "notReported", "{response}");
    assert!(content["postState"].is_null());
    assert_eq!(
        content["postStateStatus"]["status"], "unavailable",
        "{response}"
    );
    let status = serde_json::to_string(&content["postStateStatus"]).unwrap();
    assert!(status.len() <= 256);
    assert!(!status.contains("injected") && !status.contains("live-harness"));
}
