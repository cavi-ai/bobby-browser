//! Startup phase resolution: `BOBBY_MCP_TOOLSET`, then `[mcp]
//! startup_toolset`, then explore (the default).
//!
//! The point of the setting is the handshake: an agent downloads the whole
//! `tools/list` before it can call `toolset_select`, so a phase chosen after
//! connecting cannot buy back the bytes it already paid for.
//!
//! Its own test binary on purpose. These tests mutate process environment,
//! which races every other test sharing the process; keeping them alone here
//! means the only reader of `BOBBY_MCP_TOOLSET` is the code under test.

use std::sync::Arc;

use chrono::{Duration, Utc};
use interface_core::AuthorityStore;
use mcp_gateway::{Server, Toolset};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::json;
use types::{Capability, PrincipalId};
use uuid::uuid;

async fn initialized_server(build: impl FnOnce(Server) -> Server) -> Server {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000051")),
            Capability::ALL.to_vec(),
            Utc::now() + Duration::hours(1),
        )
        .await
        .expect("authority issues");
    let handle = authority
        .verify(&token.expose_once())
        .await
        .expect("bearer verifies");
    let server = build(Server::new(Arc::new(AuthenticatedRuntime::new(
        RuntimeService::default(),
        handle,
    ))));
    server
        .handle_message(json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2025-11-25","capabilities":{},
                      "clientInfo":{"name":"startup-toolset","version":"1"}}
        }))
        .await
        .expect("initialize answers");
    server
        .handle_message(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .await;
    server
}

async fn tool_names(server: &Server) -> Vec<String> {
    let response = server
        .handle_message(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
        .await
        .expect("tools/list answers");
    response["result"]["tools"]
        .as_array()
        .expect("tools is an array")
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn payload_bytes(names: &[String]) -> usize {
    serde_json::to_string(names).expect("serializable").len()
}

/// One test, because each arm mutates the same process-wide variable.
#[tokio::test]
async fn startup_phase_resolves_env_then_config_then_explore() {
    // Unset: explore — small first tools/list; widen with toolset_select.
    unsafe { std::env::remove_var("BOBBY_MCP_TOOLSET") };
    let default_names = tool_names(&initialized_server(|server| server).await).await;
    assert!(
        !default_names.contains(&"click".to_owned())
            && !default_names.contains(&"intent_fill".to_owned()),
        "the default surface should hide act and intent tools"
    );
    assert!(
        default_names.contains(&"a11y_snapshot".to_owned())
            && default_names.contains(&"toolset_select".to_owned()),
        "explore keeps read tools and toolset_select"
    );

    // Config only: the configured phase applies at connect, before the agent
    // has spent a round trip on `toolset_select`.
    let configured = tool_names(
        &initialized_server(|server| server.with_startup_toolset(Toolset::Act)).await,
    )
    .await;
    assert!(
        configured.contains(&"click".to_owned()),
        "act advertises mutating primitives"
    );
    assert!(
        !configured.contains(&"intent_fill".to_owned()),
        "act keeps intents hidden"
    );
    assert!(
        configured.contains(&"toolset_select".to_owned()),
        "a narrowed agent must always be able to widen again"
    );
    assert!(
        payload_bytes(&configured) > payload_bytes(&default_names),
        "act should advertise more than the explore default"
    );

    let full = tool_names(
        &initialized_server(|server| server.with_startup_toolset(Toolset::Full)).await,
    )
    .await;
    assert!(
        full.contains(&"intent_fill".to_owned()) && full.contains(&"click".to_owned()),
        "full carries both intent and act tools"
    );

    // Env only.
    unsafe { std::env::set_var("BOBBY_MCP_TOOLSET", "verify") };
    let from_env = tool_names(&initialized_server(|server| server).await).await;
    assert!(
        from_env.contains(&"checkpoint_save".to_owned())
            && !from_env.contains(&"intent_fill".to_owned()),
        "verify advertises the checkpoint tools and no intents"
    );

    // Env outranks config: the operator's environment is the last word.
    let both = tool_names(
        &initialized_server(|server| server.with_startup_toolset(Toolset::Explore)).await,
    )
    .await;
    assert_eq!(both, from_env, "config must not override BOBBY_MCP_TOOLSET");

    // An unparseable value falls back to the default explore surface rather
    // than failing the connection -- this selects a view, not a permission.
    unsafe { std::env::set_var("BOBBY_MCP_TOOLSET", "not-a-phase") };
    assert_eq!(
        tool_names(&initialized_server(|server| server).await).await,
        default_names,
        "an invalid BOBBY_MCP_TOOLSET should be ignored, not fatal"
    );

    unsafe { std::env::remove_var("BOBBY_MCP_TOOLSET") };
}

/// Hiding a tool must never be mistaken for revoking it: capability gates are
/// the only enforcement boundary, and a narrowed phase still dispatches.
#[test]
fn narrow_phases_hide_without_forbidding() {
    for phase in Toolset::NARROW {
        assert!(
            phase.advertises("toolset_select"),
            "{phase} must keep toolset_select so an agent can widen"
        );
        assert!(
            phase.advertises("session_close"),
            "{phase} must keep session lifecycle so an agent can clean up"
        );
    }
    assert!(!Toolset::Explore.advertises("click"));
    assert!(Toolset::Full.advertises("click"));
}
