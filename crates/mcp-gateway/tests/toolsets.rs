//! What phase selection actually costs and saves, measured against a live
//! `tools/list` rather than counted.
//!
//! Tool count is a bad proxy: `runtime_info` is a few hundred bytes and
//! `intent_follow` is 6 KB. The number that matters is the payload an agent
//! downloads on connect, so that is what this file asserts.

#![allow(dead_code)]

use std::sync::Arc;

use chrono::{Duration, Utc};
use interface_core::AuthorityStore;
use mcp_gateway::{Server, Toolset, TOOLS_LIST_BYTE_BUDGET};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use types::{Capability, PrincipalId};
use uuid::uuid;

fn all_capabilities() -> Vec<Capability> {
    vec![
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
    ]
}

async fn server() -> Server {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000044")),
            all_capabilities(),
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
                      "clientInfo":{"name":"toolsets","version":"1"}}
        }))
        .await
        .expect("initialize answers");
    server
        .handle_message(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .await;
    server
}

async fn list_tools(server: &Server) -> Vec<Value> {
    let response = server
        .handle_message(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
        .await
        .expect("tools/list answers");
    response["result"]["tools"]
        .as_array()
        .expect("tools is an array")
        .clone()
}

async fn select(server: &Server, toolset: Toolset) -> Value {
    server
        .handle_message(json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"toolset_select","arguments":{"toolset":toolset.as_str()}}
        }))
        .await
        .expect("toolset_select answers")
}

fn bytes(tools: &[Value]) -> usize {
    serde_json::to_string(tools).expect("serializable").len()
}

#[tokio::test]
async fn the_default_is_the_full_surface() {
    let server = server().await;
    let full = list_tools(&server).await;
    select(&server, Toolset::Full).await;
    assert_eq!(
        bytes(&full),
        bytes(&list_tools(&server).await),
        "selecting `full` changed a surface that was already full"
    );
}

/// The headline number. Each narrow phase has to buy back real bytes, or the
/// round trip to select one is pure cost.
#[tokio::test]
async fn every_narrow_phase_is_materially_cheaper_than_full() {
    let server = server().await;
    let full = bytes(&list_tools(&server).await);
    println!(
        "full: {full} bytes ({}% of budget)",
        full * 100 / TOOLS_LIST_BYTE_BUDGET
    );

    for phase in Toolset::NARROW {
        select(&server, phase).await;
        let narrowed = bytes(&list_tools(&server).await);
        let saved = full.saturating_sub(narrowed);
        println!(
            "{phase}: {narrowed} bytes, {saved} saved ({}% of full)",
            narrowed * 100 / full
        );
        // Two thirds, not half. `intent` is necessarily the largest narrow
        // phase: the eight `intent_*` schemas are the biggest objects on the
        // surface at 5–6 KB each, and a phase for driving through intents that
        // omitted intents would be pointless. It lands near 57%; the other two
        // are well under half.
        assert!(
            narrowed * 3 <= full * 2,
            "{phase} is {narrowed} bytes against {full} full — not a material reduction"
        );
    }
}

/// The budget is nearly consumed at `full`, so the one thing phases must never
/// do is make any view *larger*.
#[tokio::test]
async fn no_phase_exceeds_the_budget() {
    let server = server().await;
    for phase in Toolset::ALL {
        select(&server, phase).await;
        let observed = bytes(&list_tools(&server).await);
        assert!(
            observed <= TOOLS_LIST_BYTE_BUDGET,
            "{phase} is {observed} bytes, over the {TOOLS_LIST_BYTE_BUDGET} budget"
        );
    }
}

/// Names the real constraint rather than leaving it implicit in a pass:
/// `full` is within a couple of KB of the cap, so the next tool added to the
/// default surface does not fit. This fails when that becomes true, with the
/// remedy in the message, instead of the budget test failing with a byte
/// count and no explanation.
#[tokio::test]
async fn the_full_surface_has_room_for_at_least_one_more_small_tool() {
    let server = server().await;
    let full = bytes(&list_tools(&server).await);
    let headroom = TOOLS_LIST_BYTE_BUDGET.saturating_sub(full);
    println!("full: {full} bytes, {headroom} bytes of headroom");
    assert!(
        headroom >= 1_024,
        "the full surface has {headroom} bytes left. A new tool does not fit. \
         Either narrow an advertised schema, or make the tool phase-scoped so \
         it is not charged to every principal on connect."
    );
}

#[tokio::test]
async fn selecting_a_phase_changes_what_is_advertised() {
    let server = server().await;
    select(&server, Toolset::Explore).await;
    let names: Vec<String> = list_tools(&server)
        .await
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(names.contains(&"a11y_snapshot".to_owned()));
    assert!(
        !names.contains(&"click".to_owned()),
        "the explore phase advertised a mutating tool: {names:?}"
    );
}

/// Narrowing hides a tool; it must not deny it. Enforcement is the capability
/// gates' job, and a second weaker authority next to them is how a security
/// property quietly becomes advisory.
#[tokio::test]
async fn a_tool_hidden_by_the_current_phase_is_still_callable() {
    let server = server().await;
    select(&server, Toolset::Explore).await;
    let names: Vec<String> = list_tools(&server)
        .await
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    // Something `explore` genuinely hides, so the dispatch below is really
    // reaching a tool that is not advertised.
    assert!(
        !names.contains(&"checkpoint_save".to_owned()),
        "explore advertises checkpoint_save, so this proves nothing"
    );
    let response = server
        .handle_message(json!({
            "jsonrpc":"2.0","id":9,"method":"tools/call",
            "params":{"name":"checkpoint_save","arguments":{"checkpoint":{},"evidenceRefs":[]}}
        }))
        .await
        .expect("tools/call answers");
    // It fails on its arguments, not on being unavailable: an "unknown tool"
    // or "method not found" here would mean the phase became an enforcement
    // boundary.
    let message = response["error"]["message"].as_str().unwrap_or_default();
    assert!(
        !message.contains("Tool not available") && !message.contains("Method not found"),
        "a hidden tool was refused rather than dispatched: {response}"
    );
}

#[tokio::test]
async fn an_unknown_phase_name_is_rejected() {
    let server = server().await;
    let response = server
        .handle_message(json!({
            "jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"toolset_select","arguments":{"toolset":"everything"}}
        }))
        .await
        .expect("tools/call answers");
    assert!(
        response.get("error").is_some(),
        "an unknown phase name was accepted: {response}"
    );
}

/// An agent that narrows must be able to get back out, whatever phase it is in.
#[tokio::test]
async fn every_phase_can_return_to_full() {
    let server = server().await;
    let full = bytes(&list_tools(&server).await);
    for phase in Toolset::NARROW {
        select(&server, phase).await;
        select(&server, Toolset::Full).await;
        assert_eq!(
            bytes(&list_tools(&server).await),
            full,
            "could not return to the full surface from {phase}"
        );
    }
}

/// The zero-capability case, pinned here as well as in the conformance suite:
/// `toolset_select` requires no capability, so without the guard in
/// `list_tools` it would be the single tool a principal holding nothing still
/// sees.
#[tokio::test]
async fn a_principal_holding_nothing_is_shown_nothing() {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000045")),
            Vec::<Capability>::new(),
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
                      "clientInfo":{"name":"toolsets","version":"1"}}
        }))
        .await
        .expect("initialize answers");
    server
        .handle_message(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .await;
    let response = server
        .handle_message(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
        .await
        .expect("tools/list answers");
    assert!(
        response["result"]["tools"]
            .as_array()
            .is_some_and(|tools| tools.is_empty()),
        "a principal with no capabilities was shown a tool: {response}"
    );
}
