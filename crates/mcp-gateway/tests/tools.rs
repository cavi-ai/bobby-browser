use std::sync::Arc;

use chrono::{Duration, Utc};
use interface_core::{Authority, AuthorityStore, Event, EventStore};
use mcp_gateway::{ArtifactResources, Server};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use types::{
    AttemptId, CheckpointId, CommandClass, CommandEnvelope, CommandId, DismissObstructionIntent,
    Evidence, FillIntent, FillValue, FollowIntent, IntentCommand, IntentHints, LocateIntent,
    PrimitiveCommand, RuntimeCommand, SessionId, TextMatch, WaitCondition, WaitForCommand,
    WorkflowCheckpoint, WorkflowId,
};
use types::{Capability, PrincipalId};
use uuid::uuid;

async fn fixture_server(capabilities: Vec<Capability>) -> Server {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000016")),
            capabilities,
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let runtime = Arc::new(AuthenticatedRuntime::new(
        RuntimeService::default(),
        handle.clone(),
    ));
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
                "clientInfo":{"name":"test","version":"1"}
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

#[tokio::test]
async fn reinitialize_resets_the_session_lifecycle() {
    // MCP clients reconnect by sending `initialize` again; a re-initialize is
    // a session reset, not a protocol error (streamable-HTTP transports like
    // OpenClaw's bundle-mcp client call initialize on every connect).
    let server = fixture_server(vec![Capability::SessionRead]).await;
    let response = server
        .handle_message(request(
            90,
            "initialize",
            json!({
                "protocolVersion":"2025-11-25",
                "capabilities":{},
                "clientInfo":{"name":"reconnect","version":"1"}
            }),
        ))
        .await
        .expect("re-initialize returns a response");
    assert!(
        response.get("error").is_none(),
        "re-initialize is accepted: {response}"
    );
    assert_eq!(response["result"]["protocolVersion"], json!("2025-11-25"));

    // The lifecycle was genuinely reset: tool traffic before the new
    // handshake completes is rejected as not-initialized.
    let early = server
        .handle_message(request(91, "tools/list", json!({})))
        .await
        .expect("tools/list returns a response");
    assert_eq!(early["error"]["code"], json!(-32002));

    // Completing the handshake restores full traffic.
    server
        .handle_message(json!({
            "jsonrpc":"2.0","method":"notifications/initialized","params":{}
        }))
        .await;
    let list = server
        .handle_message(request(92, "tools/list", json!({})))
        .await
        .expect("tools/list after re-handshake returns a response");
    assert!(
        list.get("error").is_none(),
        "tools/list works after re-handshake: {list}"
    );
}

#[tokio::test]
async fn tools_are_capability_filtered_sorted_and_have_closed_schemas() {
    let server = fixture_server(vec![Capability::SessionRead, Capability::PageWrite]).await;
    let response = server
        .handle_message(request(2, "tools/list", json!({})))
        .await
        .unwrap();
    let tools = response["result"]["tools"].as_array().unwrap();
    let names = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        ["events_read", "page_open", "runtime_info", "session_list"]
    );
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        assert!(tool["inputSchema"]["properties"].is_object());
    }
}

#[tokio::test]
async fn runtime_info_calls_the_authenticated_runtime_and_returns_structured_content() {
    let server = fixture_server(vec![Capability::SessionRead]).await;
    let response = server
        .handle_message(request(
            3,
            "tools/call",
            json!({"name":"runtime_info","arguments":{}}),
        ))
        .await
        .unwrap();

    assert_eq!(response["id"], 3);
    assert_eq!(response["result"]["isError"], false);
    assert_eq!(
        response["result"]["structuredContent"]["active_sessions"],
        0
    );
    assert_eq!(response["result"]["content"][0]["type"], "text");
}

#[tokio::test]
async fn unavailable_or_malformed_tool_calls_fail_without_dispatch() {
    let server = fixture_server(vec![Capability::SessionRead]).await;
    let unavailable = server
        .handle_message(request(
            4,
            "tools/call",
            json!({"name":"command_execute","arguments":{}}),
        ))
        .await
        .unwrap();
    assert_eq!(unavailable["error"]["code"], -32601);

    let malformed = server
        .handle_message(request(
            5,
            "tools/call",
            json!({"name":"runtime_info","arguments":{"bearer":"do-not-accept"}}),
        ))
        .await
        .unwrap();
    assert_eq!(malformed["error"]["code"], -32602);
}

#[tokio::test]
async fn events_read_preserves_exact_event_gap_metadata() {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000017")),
            [Capability::SessionRead],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let runtime = Arc::new(AuthenticatedRuntime::new(
        RuntimeService::default(),
        handle.clone(),
    ));
    let events = EventStore::new(1);
    events.append(Event::new("one", json!({}))).await;
    events.append(Event::new("two", json!({}))).await;
    let server = Server::production(runtime, events, ArtifactResources::default());
    initialize(&server).await;

    let response = server
        .handle_message(request(
            20,
            "tools/call",
            json!({"name":"events_read","arguments":{"cursor":0,"limit":1}}),
        ))
        .await
        .unwrap();
    assert_eq!(response["error"]["code"], -32000);
    assert_eq!(
        response["error"]["data"]["eventGap"]["reason"],
        "historyLost"
    );
    assert_eq!(
        response["error"]["data"]["eventGap"]["earliestAvailable"],
        2
    );
}

#[tokio::test]
async fn revocation_after_initialize_denies_every_enumeration_and_dispatch_boundary() {
    let authority = AuthorityStore::with_capacity(1);
    let principal = PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000018"));
    let token = authority
        .issue(
            principal.clone(),
            [Capability::SessionRead, Capability::ArtifactRead],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let runtime = Arc::new(AuthenticatedRuntime::new(RuntimeService::default(), handle));
    let server = Server::new(runtime);
    initialize(&server).await;
    authority.revoke(&principal).await.unwrap();

    for (id, method, params) in [
        (31, "tools/list", json!({})),
        (
            32,
            "tools/call",
            json!({"name":"runtime_info","arguments":{}}),
        ),
        (33, "resources/list", json!({})),
        (34, "resources/read", json!({"uri":"artifact://deadbeef"})),
    ] {
        let response = server
            .handle_message(request(id, method, params))
            .await
            .unwrap();
        assert_eq!(
            response["error"]["data"]["interfaceError"]["code"], "authenticationFailed",
            "{response}"
        );
        let serialized = response.to_string();
        assert!(!serialized.contains("runtime_info"));
        assert!(!serialized.contains("deadbeef"));
    }
}

#[tokio::test]
async fn expiry_after_initialize_denies_tool_enumeration() {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000019")),
            [Capability::SessionRead],
            Utc::now() + Duration::milliseconds(100),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let server = Server::new(Arc::new(AuthenticatedRuntime::new(
        RuntimeService::default(),
        handle,
    )));
    initialize(&server).await;
    tokio::time::sleep(std::time::Duration::from_millis(125)).await;
    let response = server
        .handle_message(request(40, "tools/list", json!({})))
        .await
        .unwrap();
    assert_eq!(
        response["error"]["data"]["interfaceError"]["code"],
        "authenticationFailed"
    );
    assert!(!response.to_string().contains("runtime_info"));
}

#[tokio::test]
async fn unrequested_methods_list_extension_is_not_exposed() {
    let server = fixture_server(vec![Capability::SessionRead]).await;
    let response = server
        .handle_message(request(45, "methods/list", json!({})))
        .await
        .unwrap();
    assert_eq!(response["error"]["code"], -32601, "{response}");
}

#[tokio::test]
async fn command_and_checkpoint_schemas_are_fully_nested_and_match_pre_dispatch_bounds() {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000020")),
            [Capability::BrowserMutate, Capability::RecoveryWrite],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let runtime = Arc::new(AuthenticatedRuntime::new(RuntimeService::default(), handle));
    let server = Server::new(runtime.clone());
    initialize(&server).await;
    let listed = server
        .handle_message(request(50, "tools/list", json!({})))
        .await
        .unwrap();
    let tools = listed["result"]["tools"].as_array().unwrap();
    for tool in tools {
        assert_closed_typed_objects(&tool["inputSchema"]);
    }
    let command_schema = tools
        .iter()
        .find(|tool| tool["name"] == "command_execute")
        .unwrap();
    assert_eq!(
        command_schema["inputSchema"]["$defs"]["PrimitiveCommand"]["oneOf"]
            .as_array()
            .unwrap()
            .len(),
        18
    );
    let runtime_command = &command_schema["inputSchema"]["$defs"]["RuntimeCommand"]["oneOf"];
    assert_eq!(
        runtime_command.as_array().unwrap().len(),
        2,
        "{runtime_command}"
    );
    assert_eq!(
        command_schema["inputSchema"]["$defs"]["CommandEnvelope"]["properties"]["command"]["$ref"],
        "#/$defs/RuntimeCommand"
    );
    assert_eq!(
        command_schema["inputSchema"]["$defs"]["IntentCommand"]["oneOf"]
            .as_array()
            .unwrap()
            .len(),
        8
    );
    // Must match `crates/types/src/outcomes.rs`'s `Evidence` enum variant-for-variant: a
    // hand-listed schema that silently drops a variant (as `Configuration`,
    // `BrowserExecution`, and `JavaScriptResult` previously were) makes
    // `checkpoint_save` reject any evidence array containing that variant with
    // `INVALID_PARAMS`, even though the type itself round-trips fine.
    let evidence_variants = command_schema["inputSchema"]["$defs"]["Evidence"]["oneOf"]
        .as_array()
        .unwrap();
    assert_eq!(evidence_variants.len(), 18, "{evidence_variants:?}");
    let evidence_kinds = evidence_variants
        .iter()
        .map(|variant| variant["properties"]["kind"]["const"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(
        evidence_kinds.contains(&"javaScriptResult"),
        "{evidence_kinds:?}"
    );
    assert!(
        evidence_kinds.contains(&"intentExecution"),
        "{evidence_kinds:?}"
    );
    assert!(evidence_kinds.contains(&"extraction"), "{evidence_kinds:?}");

    let checkpoint_schema = tools
        .iter()
        .find(|tool| tool["name"] == "checkpoint_save")
        .unwrap();
    assert_eq!(
        checkpoint_schema["inputSchema"]["$defs"]["WorkflowCheckpoint"]["properties"]
            ["recoveryReceipts"]["maxItems"],
        0
    );

    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: None,
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Primitive(PrimitiveCommand::ListPages(types::ListPagesCommand)),
    };
    let mut envelope_value = serde_json::to_value(envelope).unwrap();
    envelope_value["command"]["unexpected"] = json!(true);
    let rejected = server.handle_message(request(51, "tools/call", json!({
        "name":"command_execute","arguments":{"envelope":envelope_value,"idempotencyKey":"nested-extra"}
    }))).await.unwrap();
    assert_eq!(rejected["error"]["code"], -32602, "{rejected}");
    assert_eq!(runtime.submit_dispatch_count(), 0);

    let long_attribute = "a".repeat(129);
    let bounded_envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: None,
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Primitive(PrimitiveCommand::TypeText(types::TypeTextCommand {
            selector: "#name".to_owned(),
            target: Some(types::TargetSpec {
                attributes: [(long_attribute, "value".to_owned())].into_iter().collect(),
                ..Default::default()
            }),
            value: "Ada".to_owned(),
            clear_first: true,
        })),
    };
    let rejected = server
        .handle_message(request(
            53,
            "tools/call",
            json!({
                "name":"command_execute",
                "arguments":{"envelope":bounded_envelope,"idempotencyKey":"long-property-name"}
            }),
        ))
        .await
        .unwrap();
    assert_eq!(rejected["error"]["code"], -32602, "{rejected}");
    assert_eq!(runtime.submit_dispatch_count(), 0);

    let checkpoint = WorkflowCheckpoint {
        schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
        checkpoint_id: CheckpointId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: types::PageId::new(),
        restart_url: "https://example.test/".to_owned(),
        current_url: "https://example.test/".to_owned(),
        cursor: None,
        boundary_command_id: None,
        recovery_class: CommandClass::Replayable,
        invariants: vec![],
        replayable_inputs: vec![],
        evidence: vec![],
        recovery_history: vec![],
        recovery_receipts: vec![],
        created_at: Utc::now(),
    };
    let oversized = vec![
        Evidence::Navigation {
            url: "https://example.test/".to_owned(),
            title: "fixture".to_owned()
        };
        129
    ];
    let rejected = server
        .handle_message(request(
            52,
            "tools/call",
            json!({
                "name":"checkpoint_save","arguments":{"checkpoint":checkpoint,"evidence":oversized}
            }),
        ))
        .await
        .unwrap();
    assert_eq!(rejected["error"]["code"], -32602, "{rejected}");
    assert_eq!(runtime.checkpoint_dispatch_count(), 0);
}

// F6: `session_create`'s MCP schema exposes an optional `executionPolicy` object that
// maps into `CreateSessionRequest.execution_policy`. These two tests prove both sides of
// deny-by-default at the MCP surface: an explicit grant is honored, and an omitted field
// falls back to `ExecutionPolicy::default()` (deny).

#[tokio::test]
async fn session_create_with_execution_policy_grants_javascript_evaluation_on_the_stored_session() {
    let server = fixture_server(vec![Capability::SessionWrite]).await;
    let response = server
        .handle_message(request(
            60,
            "tools/call",
            json!({
                "name":"session_create",
                "arguments":{
                    "profile":"fixture",
                    "executionPolicy":{"javascriptEvaluation":true}
                }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response["result"]["isError"], false, "{response}");
    assert_eq!(
        response["result"]["structuredContent"]["execution_policy"]["javascriptEvaluation"], true,
        "{response}"
    );
}

#[tokio::test]
async fn session_create_without_execution_policy_denies_javascript_evaluation_by_default() {
    let server = fixture_server(vec![Capability::SessionWrite]).await;
    let response = server
        .handle_message(request(
            61,
            "tools/call",
            json!({
                "name":"session_create",
                "arguments":{"profile":"fixture"}
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response["result"]["isError"], false, "{response}");
    assert_eq!(
        response["result"]["structuredContent"]["execution_policy"]["javascriptEvaluation"], false,
        "{response}"
    );
}

// Regression: `evidence_variants()` previously hand-listed only 12 of `Evidence`'s 15
// variants, so `checkpoint_save`'s `evidence` array (which schema-validates against
// `$defs/Evidence`) rejected any workflow that ran `evaluateJavaScript` and then tried
// to checkpoint the resulting `Evidence::JavaScriptResult` — pre-dispatch, with
// `INVALID_PARAMS`, before `runtime.checkpoint` was ever called.
#[tokio::test]
async fn checkpoint_save_schema_accepts_javascript_and_actionable_accessibility_evidence() {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000021")),
            [Capability::RecoveryWrite],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let runtime = Arc::new(AuthenticatedRuntime::new(RuntimeService::default(), handle));
    let server = Server::new(runtime.clone());
    initialize(&server).await;

    let checkpoint = WorkflowCheckpoint {
        schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
        checkpoint_id: CheckpointId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: types::PageId::new(),
        restart_url: "https://example.test/".to_owned(),
        current_url: "https://example.test/".to_owned(),
        cursor: None,
        boundary_command_id: None,
        recovery_class: CommandClass::Reconciliable,
        invariants: vec![],
        replayable_inputs: vec![],
        evidence: vec![],
        recovery_history: vec![],
        recovery_receipts: vec![],
        created_at: Utc::now(),
    };
    let evidence = vec![
        Evidence::JavaScriptResult {
            value: json!({"answer": 42}),
            truncated: false,
        },
        Evidence::AccessibilitySnapshot {
            page_id: checkpoint.page_id.clone(),
            nodes: vec![types::AccessibilityNode {
                role: Some("textbox".into()),
                name: Some("Email".into()),
                target: Some(types::AccessibilityTarget {
                    role: "textbox".into(),
                    accessible_name: "Email".into(),
                    ordinal: Some(1),
                }),
                ..types::AccessibilityNode::default()
            }],
            truncated: false,
        },
    ];

    let response = server
        .handle_message(request(
            54,
            "tools/call",
            json!({
                "name":"checkpoint_save",
                "arguments":{"checkpoint":checkpoint,"evidence":evidence}
            }),
        ))
        .await
        .unwrap();

    // `RuntimeService::default()` has no `RecoveryCoordinator`, so the call still fails
    // downstream (an interface error, not schema rejection) — the proof here is that
    // validation let it through to dispatch at all: -32602 would mean the schema
    // rejected the `javaScriptResult` evidence item before `runtime.checkpoint` ran.
    assert_ne!(response["error"]["code"], -32602, "{response}");
    assert_eq!(
        runtime.checkpoint_dispatch_count(),
        1,
        "schema validation must accept JavaScript and actionable accessibility evidence and reach \
         dispatch: {response}"
    );
}

#[tokio::test]
async fn command_execute_schema_accepts_locate_intent_envelope() {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000022")),
            [Capability::BrowserMutate, Capability::IntentExecute],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let runtime = Arc::new(AuthenticatedRuntime::new(RuntimeService::default(), handle));
    let server = Server::new(runtime);
    initialize(&server).await;

    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: None,
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Intent(IntentCommand::Locate(LocateIntent {
            purpose: "Continue".to_owned(),
            hints: IntentHints::default(),
        })),
    };

    let response = server
        .handle_message(request(
            70,
            "tools/call",
            json!({
                "name":"command_execute",
                "arguments":{"envelope":envelope}
            }),
        ))
        .await
        .unwrap();

    // Downstream may fail (unknown session, etc.); schema must not reject with INVALID_PARAMS.
    assert_ne!(response["error"]["code"], -32602, "{response}");
}

#[tokio::test]
async fn command_execute_schema_accepts_fill_intent_with_snapshot_ordinal() {
    let server = fixture_server(vec![Capability::BrowserMutate, Capability::IntentExecute]).await;
    initialize(&server).await;
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: None,
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Intent(IntentCommand::Fill(FillIntent {
            purpose: "enter the applicant name".into(),
            hints: IntentHints {
                role: Some("textbox".into()),
                near_text: Some(TextMatch::Exact("Name".into())),
                ..IntentHints::default()
            },
            value: FillValue::Text {
                text: "Ada".into(),
                clear_first: true,
            },
        })),
    };
    let mut envelope_value = serde_json::to_value(&envelope).unwrap();
    envelope_value["command"]["input"]["input"]["hints"]["ordinal"] = json!(1);
    let response = server
        .handle_message(request(
            71,
            "tools/call",
            json!({
                "name":"command_execute", "arguments":{"envelope":envelope_value}
            }),
        ))
        .await
        .unwrap();
    assert_ne!(
        response["error"]["code"], -32602,
        "{response}; envelope={envelope_value}"
    );
}

#[tokio::test]
async fn command_execute_schema_rejects_locate_purpose_over_256() {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000023")),
            [Capability::BrowserMutate, Capability::IntentExecute],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let runtime = Arc::new(AuthenticatedRuntime::new(RuntimeService::default(), handle));
    let server = Server::new(runtime.clone());
    initialize(&server).await;

    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: None,
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Intent(IntentCommand::Locate(LocateIntent {
            purpose: "Continue".to_owned(),
            hints: IntentHints::default(),
        })),
    };
    let mut envelope_value = serde_json::to_value(envelope).unwrap();
    envelope_value["command"]["input"]["input"]["purpose"] = json!("a".repeat(257));

    let rejected = server
        .handle_message(request(
            71,
            "tools/call",
            json!({
                "name":"command_execute",
                "arguments":{"envelope":envelope_value}
            }),
        ))
        .await
        .unwrap();
    assert_eq!(rejected["error"]["code"], -32602, "{rejected}");
    assert_eq!(runtime.submit_dispatch_count(), 0);
}

#[tokio::test]
async fn command_execute_schema_accepts_follow_intent_envelope() {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000024")),
            [Capability::BrowserMutate, Capability::IntentExecute],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let runtime = Arc::new(AuthenticatedRuntime::new(RuntimeService::default(), handle));
    let server = Server::new(runtime);
    initialize(&server).await;

    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: None,
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Intent(IntentCommand::Follow(FollowIntent {
            purpose: "Details".to_owned(),
            hints: IntentHints::default(),
            expected_destination: WaitForCommand {
                condition: WaitCondition::Url {
                    matcher: TextMatch::Contains("/details".into()),
                },
                timeout_ms: 5_000,
            },
            boundary: false,
        })),
    };

    let response = server
        .handle_message(request(
            72,
            "tools/call",
            json!({
                "name":"command_execute",
                "arguments":{"envelope":envelope}
            }),
        ))
        .await
        .unwrap();

    // Downstream may fail (unknown session, etc.); schema must not reject with INVALID_PARAMS.
    assert_ne!(response["error"]["code"], -32602, "{response}");
}

#[tokio::test]
async fn command_execute_schema_rejects_follow_missing_expected_destination() {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000025")),
            [Capability::BrowserMutate, Capability::IntentExecute],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let runtime = Arc::new(AuthenticatedRuntime::new(RuntimeService::default(), handle));
    let server = Server::new(runtime.clone());
    initialize(&server).await;

    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: None,
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Intent(IntentCommand::Follow(FollowIntent {
            purpose: "Details".to_owned(),
            hints: IntentHints::default(),
            expected_destination: WaitForCommand {
                condition: WaitCondition::Url {
                    matcher: TextMatch::Contains("/details".into()),
                },
                timeout_ms: 5_000,
            },
            boundary: false,
        })),
    };
    let mut envelope_value = serde_json::to_value(envelope).unwrap();
    envelope_value["command"]["input"]["input"]
        .as_object_mut()
        .unwrap()
        .remove("expectedDestination");

    let rejected = server
        .handle_message(request(
            73,
            "tools/call",
            json!({
                "name":"command_execute",
                "arguments":{"envelope":envelope_value}
            }),
        ))
        .await
        .unwrap();
    assert_eq!(rejected["error"]["code"], -32602, "{rejected}");
    assert_eq!(runtime.submit_dispatch_count(), 0);
}

#[tokio::test]
async fn command_execute_schema_accepts_dismiss_obstruction_intent_envelope() {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000026")),
            [Capability::BrowserMutate, Capability::IntentExecute],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let runtime = Arc::new(AuthenticatedRuntime::new(RuntimeService::default(), handle));
    let server = Server::new(runtime);
    initialize(&server).await;

    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: None,
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Intent(IntentCommand::DismissObstruction(
            DismissObstructionIntent {
                purpose: "Cookie notice close button".to_owned(),
                hints: IntentHints::default(),
                timeout_ms: 5_000,
            },
        )),
    };

    let response = server
        .handle_message(request(
            74,
            "tools/call",
            json!({
                "name":"command_execute",
                "arguments":{"envelope":envelope}
            }),
        ))
        .await
        .unwrap();

    // Downstream may fail (unknown session, etc.); schema must not reject with INVALID_PARAMS.
    assert_ne!(response["error"]["code"], -32602, "{response}");
}

#[tokio::test]
async fn command_execute_schema_rejects_dismiss_obstruction_missing_purpose() {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000027")),
            [Capability::BrowserMutate, Capability::IntentExecute],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let runtime = Arc::new(AuthenticatedRuntime::new(RuntimeService::default(), handle));
    let server = Server::new(runtime.clone());
    initialize(&server).await;

    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: None,
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Intent(IntentCommand::DismissObstruction(
            DismissObstructionIntent {
                purpose: "Cookie notice close button".to_owned(),
                hints: IntentHints::default(),
                timeout_ms: 5_000,
            },
        )),
    };
    let mut envelope_value = serde_json::to_value(envelope).unwrap();
    envelope_value["command"]["input"]["input"]
        .as_object_mut()
        .unwrap()
        .remove("purpose");

    let rejected = server
        .handle_message(request(
            75,
            "tools/call",
            json!({
                "name":"command_execute",
                "arguments":{"envelope":envelope_value}
            }),
        ))
        .await
        .unwrap();
    assert_eq!(rejected["error"]["code"], -32602, "{rejected}");
    assert_eq!(runtime.submit_dispatch_count(), 0);
}

#[tokio::test]
async fn command_execute_schema_accepts_extract_intent_envelope() {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000028")),
            [Capability::BrowserMutate, Capability::IntentExecute],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let runtime = Arc::new(AuthenticatedRuntime::new(RuntimeService::default(), handle));
    let server = Server::new(runtime);
    initialize(&server).await;

    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: None,
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Intent(IntentCommand::Extract(types::ExtractIntent {
            purpose: "Profile summary".to_owned(),
            fields: vec![
                types::ExtractField {
                    name: "displayName".to_owned(),
                    purpose: "Display name".to_owned(),
                    hints: IntentHints::default(),
                    value: types::ExtractValueKind::Text,
                },
                types::ExtractField {
                    name: "profileLink".to_owned(),
                    purpose: "Profile link".to_owned(),
                    hints: IntentHints::default(),
                    value: types::ExtractValueKind::Href,
                },
            ],
        })),
    };

    let response = server
        .handle_message(request(
            76,
            "tools/call",
            json!({
                "name":"command_execute",
                "arguments":{"envelope":envelope}
            }),
        ))
        .await
        .unwrap();

    // Downstream may fail (unknown session, etc.); schema must not reject with INVALID_PARAMS.
    assert_ne!(response["error"]["code"], -32602, "{response}");
}

#[tokio::test]
async fn command_execute_schema_rejects_extract_with_empty_fields() {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000029")),
            [Capability::BrowserMutate, Capability::IntentExecute],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let runtime = Arc::new(AuthenticatedRuntime::new(RuntimeService::default(), handle));
    let server = Server::new(runtime.clone());
    initialize(&server).await;

    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: None,
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Intent(IntentCommand::Extract(types::ExtractIntent {
            purpose: "Profile summary".to_owned(),
            fields: vec![types::ExtractField {
                name: "displayName".to_owned(),
                purpose: "Display name".to_owned(),
                hints: IntentHints::default(),
                value: types::ExtractValueKind::Text,
            }],
        })),
    };
    let mut envelope_value = serde_json::to_value(envelope).unwrap();
    envelope_value["command"]["input"]["input"]["fields"] = json!([]);

    let rejected = server
        .handle_message(request(
            77,
            "tools/call",
            json!({
                "name":"command_execute",
                "arguments":{"envelope":envelope_value}
            }),
        ))
        .await
        .unwrap();
    assert_eq!(rejected["error"]["code"], -32602, "{rejected}");
    assert_eq!(runtime.submit_dispatch_count(), 0);
}

fn assert_closed_typed_objects(schema: &Value) {
    match schema {
        Value::Array(values) => values.iter().for_each(assert_closed_typed_objects),
        Value::Object(values) => {
            if values.get("type") == Some(&json!("object")) && values.contains_key("properties") {
                assert_eq!(
                    values.get("additionalProperties"),
                    Some(&json!(false)),
                    "{schema}"
                );
                assert!(
                    values.get("required").is_some_and(Value::is_array),
                    "{schema}"
                );
            }
            values.values().for_each(assert_closed_typed_objects);
        }
        _ => {}
    }
}

#[tokio::test]
async fn flat_browser_tools_are_listed_and_follow_capability_grants() {
    let all = [
        Capability::BrowserMutate,
        Capability::FileDownload,
        Capability::FileUpload,
        Capability::JavascriptEvaluate,
    ];
    let server = fixture_server(all.to_vec()).await;
    let listed = server
        .handle_message(request(70, "tools/list", json!({})))
        .await
        .unwrap();
    let names = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    for expected in [
        "navigate",
        "click",
        "type_text",
        "inspect",
        "screenshot",
        "wait_for",
        "page_list",
        "page_close",
        "page_activate",
        "a11y_snapshot",
        "download_url",
        "upload_files",
        "evaluate_javascript",
    ] {
        assert!(
            names.contains(&expected.to_owned()),
            "missing {expected}: {names:?}"
        );
    }
    let navigate = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "navigate")
        .unwrap();
    assert_eq!(
        navigate["inputSchema"]["required"],
        json!(["sessionId", "pageId", "url"])
    );
    let click = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "click")
        .unwrap();
    assert_eq!(
        click["inputSchema"]["required"],
        json!(["sessionId", "pageId"]),
        "semantic snapshot targets must not require a legacy CSS selector"
    );
    let type_text = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "type_text")
        .unwrap();
    assert_eq!(
        type_text["inputSchema"]["required"],
        json!(["sessionId", "pageId", "value"]),
        "semantic snapshot targets must not require a legacy CSS selector"
    );
    let upload_files = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "upload_files")
        .unwrap();
    assert_eq!(
        upload_files["inputSchema"]["required"],
        json!(["sessionId", "pageId", "paths"]),
        "semantic snapshot targets must not require a legacy CSS selector"
    );

    let mutate_only = fixture_server(vec![Capability::BrowserMutate]).await;
    let listed = mutate_only
        .handle_message(request(71, "tools/list", json!({})))
        .await
        .unwrap();
    let names = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    for visible in [
        "navigate",
        "click",
        "type_text",
        "inspect",
        "screenshot",
        "wait_for",
        "page_list",
        "page_close",
        "page_activate",
        "a11y_snapshot",
    ] {
        assert!(
            names.contains(&visible.to_owned()),
            "{visible} must be visible: {names:?}"
        );
    }
    for hidden in ["download_url", "upload_files", "evaluate_javascript"] {
        assert!(
            !names.contains(&hidden.to_owned()),
            "{hidden} must be hidden: {names:?}"
        );
    }
    let denied = mutate_only
        .handle_message(request(72, "tools/call", json!({
            "name":"download_url",
            "arguments":{"sessionId":SessionId::new().0.to_string(),"url":"https://example.test/file","maxBytes":1024}
        })))
        .await
        .unwrap();
    assert_eq!(denied["error"]["code"], -32601, "{denied}");
}

#[tokio::test]
async fn flat_browser_tools_validate_arguments_and_submit_envelopes() {
    let runtime = Arc::new(authenticated_with_browser_mutate().await);
    let server = Server::new(runtime);
    initialize(&server).await;

    let missing_url = server
        .handle_message(request(80, "tools/call", json!({
            "name":"navigate",
            "arguments":{"sessionId":SessionId::new().0.to_string(),"pageId":types::PageId::new().0.to_string()}
        })))
        .await
        .unwrap();
    assert_eq!(missing_url["error"]["code"], -32602, "{missing_url}");

    let unknown_field = server
        .handle_message(request(81, "tools/call", json!({
            "name":"click",
            "arguments":{"sessionId":SessionId::new().0.to_string(),"pageId":types::PageId::new().0.to_string(),"selector":"#go","surprise":true}
        })))
        .await
        .unwrap();
    assert_eq!(unknown_field["error"]["code"], -32602, "{unknown_field}");

    let navigated = server
        .handle_message(request(
            82,
            "tools/call",
            json!({
                "name":"navigate",
                "arguments":{
                    "sessionId":SessionId::new().0.to_string(),
                    "pageId":types::PageId::new().0.to_string(),
                    "url":"https://example.test/",
                    "waitUntil":"interactive",
                    "timeoutMs":5000
                }
            }),
        ))
        .await
        .unwrap();
    assert!(navigated["error"].is_null(), "{navigated}");
    assert_eq!(navigated["result"]["isError"], json!(false));
    assert_eq!(
        navigated["result"]["structuredContent"]["status"],
        json!("failed"),
        "{navigated}"
    );
    assert!(navigated["result"]["structuredContent"]["commandId"].is_string());

    let evaluated = server
        .handle_message(request(
            83,
            "tools/call",
            json!({
                "name":"evaluate_javascript",
                "arguments":{
                    "sessionId":SessionId::new().0.to_string(),
                    "pageId":types::PageId::new().0.to_string(),
                    "expression":"document.title"
                }
            }),
        ))
        .await
        .unwrap();
    assert!(evaluated["error"].is_null(), "{evaluated}");
    assert!(evaluated["result"]["structuredContent"]["commandId"].is_string());
}

async fn authenticated_with_browser_mutate() -> AuthenticatedRuntime {
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000030")),
            [
                Capability::BrowserMutate,
                Capability::FileDownload,
                Capability::FileUpload,
                Capability::JavascriptEvaluate,
            ],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    AuthenticatedRuntime::new(RuntimeService::default(), handle)
}

#[tokio::test]
async fn session_close_deletes_an_owned_session() {
    let server = fixture_server(vec![Capability::SessionWrite]).await;
    let created = server
        .handle_message(request(
            90,
            "tools/call",
            json!({
                "name":"session_create","arguments":{"profile":"close-me"}
            }),
        ))
        .await
        .unwrap();
    let session_id = created["result"]["structuredContent"]["id"]
        .as_str()
        .unwrap();

    let closed = server
        .handle_message(request(
            91,
            "tools/call",
            json!({
                "name":"session_close","arguments":{"sessionId":session_id}
            }),
        ))
        .await
        .unwrap();
    assert!(closed["error"].is_null(), "{closed}");
    assert_eq!(closed["result"]["structuredContent"]["closed"], json!(true));

    let missing = server
        .handle_message(request(
            92,
            "tools/call",
            json!({
                "name":"session_close","arguments":{"sessionId":session_id}
            }),
        ))
        .await
        .unwrap();
    assert_eq!(missing["error"]["code"], -32000, "{missing}");
}
