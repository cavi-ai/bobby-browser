use std::sync::Arc;

mod common;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use interface_core::{Authority, AuthorityStore, Event, EventStore};
use mcp_gateway::{ArtifactResources, Server};
use page_runtime::PageRuntime;
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use session_manager::SessionManager;
use types::{
    AttemptId, CheckpointId, CommandClass, CommandEnvelope, CommandId, CompleteFormField,
    CompleteFormIntent, ControlAction, DismissObstructionIntent, Evidence, FillIntent,
    FollowIntent, IntentCommand, IntentHints, LocateIntent, PageId, PrimitiveCommand,
    RuntimeCommand, SessionId, TextMatch, WaitCondition, WaitForCommand, WorkflowCheckpoint,
    WorkflowId,
};
use types::{Capability, PrincipalId};
use uuid::uuid;
use worker_pool::{BrowserWorker, WorkerFactory, WorkerPool};
use workflow_journal::{CommandJournal, JournalRecord, JsonlJournal};

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
    let server = Server::new(runtime)
        .with_startup_toolset(mcp_gateway::Toolset::Full)
        .with_jobs({
            let (port, _scheduler) = mcp_gateway::InProcessJobPort::memory();
            Arc::new(port)
        });
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
    // MCP clients reconnect by sending `initialize` again; a re-initialize is a
    // session reset, not a protocol error.
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

    // Tool traffic before the new handshake completes is rejected as
    // not-initialized.
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
    // `toolset_select` requires no capability: it changes what this connection
    // is shown, not what it may do, and gating it would let a principal narrow
    // into a phase it lacks the capability to leave.
    assert_eq!(
        names,
        [
            "events_read",
            "page_open",
            "runtime_info",
            "session_list",
            "toolset_select"
        ]
    );
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        assert!(tool["inputSchema"]["properties"].is_object());
    }
}

#[tokio::test]
async fn session_create_advertises_only_execution_policies_the_principal_can_grant() {
    let limited = fixture_server(vec![Capability::SessionWrite]).await;
    let limited_list = limited
        .handle_message(request(2, "tools/list", json!({})))
        .await
        .unwrap();
    let limited_session_create = limited_list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "session_create")
        .unwrap();
    let limited_policy =
        &limited_session_create["inputSchema"]["properties"]["executionPolicy"]["properties"];
    assert!(limited_policy.get("fingerprint").is_none());
    assert!(limited_policy.get("humanize").is_none());

    let privileged = fixture_server(vec![
        Capability::SessionWrite,
        Capability::BrowserFingerprint,
        Capability::BrowserHumanize,
    ])
    .await;
    let privileged_list = privileged
        .handle_message(request(3, "tools/list", json!({})))
        .await
        .unwrap();
    let privileged_session_create = privileged_list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "session_create")
        .unwrap();
    let privileged_policy =
        &privileged_session_create["inputSchema"]["properties"]["executionPolicy"]["properties"];
    assert_eq!(privileged_policy["fingerprint"]["type"], "boolean");
    assert_eq!(privileged_policy["humanize"]["type"], "boolean");
}

#[tokio::test]
async fn page_open_with_url_requires_browser_mutate_before_opening_a_page() {
    let server = fixture_server(vec![Capability::SessionWrite, Capability::PageWrite]).await;
    let created = server
        .handle_message(request(
            2,
            "tools/call",
            json!({"name":"session_create","arguments":{"profile":"fixture"}}),
        ))
        .await
        .unwrap();
    let session_id = created["result"]["structuredContent"]["id"].clone();

    let denied = server
        .handle_message(request(
            3,
            "tools/call",
            json!({
                "name":"page_open",
                "arguments":{"sessionId":session_id,"url":"https://example.test/jobs"}
            }),
        ))
        .await
        .unwrap();

    assert_eq!(
        denied["error"]["data"]["interfaceError"]["code"],
        json!("missingCapability")
    );
    assert_eq!(
        denied["error"]["data"]["interfaceError"]["requiredCapability"],
        json!("browser:mutate")
    );
    // RPC-layer failures carry the machine-readable repair hint next to the
    // interface error, so an agent can act without reading the taxonomy first.
    assert_eq!(
        denied["error"]["data"]["repair"]["doc"],
        json!("bobby://failure-taxonomy")
    );
    assert!(
        denied["error"]["data"]["repair"]["action"]
            .as_str()
            .unwrap()
            .contains("requiredCapability"),
        "{denied}"
    );
}

#[tokio::test]
async fn control_action_accepts_a_two_field_snapshot_target() {
    let server = fixture_server(Capability::ALL.to_vec()).await;
    let created = server
        .handle_message(request(
            2,
            "tools/call",
            json!({"name":"session_create","arguments":{"profile":"fixture"}}),
        ))
        .await
        .unwrap();
    let session_id = created["result"]["structuredContent"]["id"].clone();
    let page_id = json!("00000000-0000-4000-8000-000000000099");

    // role + accessibleName only: ordinal/framePath/shadowPath default. The
    // call must clear schema validation; the runtime then reports the unknown
    // page, not a malformed-arguments rejection.
    let outcome = server
        .handle_message(request(
            3,
            "tools/call",
            json!({
                "name":"control_action",
                "arguments":{
                    "sessionId":session_id,
                    "pageId":page_id,
                    "target":{"role":"button","accessibleName":"Save priority"},
                    "action":{"kind":"activate"}
                }
            }),
        ))
        .await
        .unwrap();
    let code = outcome["error"]["data"]["interfaceError"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        code != "invalidRequest" && code != "malformedArguments",
        "two-field target was rejected at the schema layer: {outcome}"
    );

    // accessibleName is still required.
    let rejected = server
        .handle_message(request(
            4,
            "tools/call",
            json!({
                "name":"control_action",
                "arguments":{
                    "sessionId":session_id,
                    "pageId":page_id,
                    "target":{"role":"button"},
                    "action":{"kind":"activate"}
                }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(
        rejected["error"]["data"]["reason"],
        json!("schemaViolation"),
        "target without accessibleName must be rejected: {rejected}"
    );
}

#[tokio::test]
async fn page_open_closes_the_new_page_when_navigation_returns_an_interface_error() {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-0000000000d1")),
            [
                Capability::SessionRead,
                Capability::SessionWrite,
                Capability::PageWrite,
                Capability::BrowserMutate,
            ],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let live = common::live_server_failing_navigation_interface(handle).await;
    common::initialize(&live.server).await;

    let created = live
        .server
        .handle_message(request(
            2,
            "tools/call",
            json!({"name":"session_create","arguments":{"profile":"fixture"}}),
        ))
        .await
        .unwrap();
    let session_id = created["result"]["structuredContent"]["id"].clone();
    let failed = live
        .server
        .handle_message(request(
            3,
            "tools/call",
            json!({
                "name":"page_open",
                "arguments":{"sessionId":session_id,"url":"https://example.test/fails"}
            }),
        ))
        .await
        .unwrap();
    assert_eq!(
        failed["error"]["data"]["interfaceError"]["code"],
        json!("internal"),
        "the injected navigation failure must reach the caller: {failed}"
    );

    let listed = live
        .server
        .handle_message(request(
            4,
            "tools/call",
            json!({"name":"session_list","arguments":{}}),
        ))
        .await
        .unwrap();
    assert_eq!(
        listed["result"]["structuredContent"]["sessions"][0]["page_ids"],
        json!([]),
        "a failed page_open must not leave an unreachable page behind: {listed}"
    );
}

#[tokio::test]
async fn page_close_removes_the_page_from_session_state() {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-0000000000d2")),
            [
                Capability::SessionRead,
                Capability::SessionWrite,
                Capability::PageWrite,
                Capability::BrowserMutate,
            ],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let live = common::live_server(handle).await;
    common::initialize(&live.server).await;
    let mut next_id = 10;
    let (session_id, page_id) = common::create_session_and_page(&live.server, &mut next_id).await;

    let closed = live
        .server
        .handle_message(request(
            12,
            "tools/call",
            json!({
                "name":"page_close",
                "arguments":{"sessionId":session_id,"pageId":page_id}
            }),
        ))
        .await
        .unwrap();
    assert_eq!(closed["result"]["structuredContent"]["status"], "completed");

    let listed = live
        .server
        .handle_message(request(
            13,
            "tools/call",
            json!({"name":"session_list","arguments":{}}),
        ))
        .await
        .unwrap();
    assert_eq!(
        listed["result"]["structuredContent"]["sessions"][0]["page_ids"],
        json!([]),
        "page_close must keep session metadata in sync: {listed}"
    );
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
        (35, "prompts/list", json!({})),
        (
            36,
            "prompts/get",
            json!({"name":"fill_and_submit_form","arguments":{"sessionId":"s-1","pageId":"p-1"}}),
        ),
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
async fn command_schema_validates_the_full_union_but_advertises_an_opaque_command() {
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
    // `tools/list` defaults to the narrow explore phase, which advertises
    // neither of these tools. This asserts a schema's shape, not the default
    // phase's membership, so widen first.
    server
        .handle_message(request(
            49,
            "tools/call",
            json!({"name":"toolset_select","arguments":{"toolset":"full"}}),
        ))
        .await
        .unwrap();
    let listed = server
        .handle_message(request(50, "tools/list", json!({})))
        .await
        .unwrap();
    let tools = listed["result"]["tools"].as_array().unwrap();
    for tool in tools {
        assert_closed_typed_objects(&tool["inputSchema"]);
    }

    // `tool_schema` is what `validate_tool_arguments` enforces at dispatch, so it
    // must keep the full command union regardless of what `tools/list` advertises.
    let validation_schema = mcp_gateway::schema_for_test("command_execute");
    assert_eq!(
        validation_schema["$defs"]["PrimitiveCommand"]["oneOf"]
            .as_array()
            .unwrap()
            .len(),
        27
    );
    let runtime_command = &validation_schema["$defs"]["RuntimeCommand"]["oneOf"];
    assert_eq!(
        runtime_command.as_array().unwrap().len(),
        2,
        "{runtime_command}"
    );
    assert_eq!(
        validation_schema["$defs"]["CommandEnvelope"]["properties"]["command"]["$ref"],
        "#/$defs/RuntimeCommand"
    );
    assert_eq!(
        validation_schema["$defs"]["IntentCommand"]["oneOf"]
            .as_array()
            .unwrap()
            .len(),
        9
    );

    // In `tools/list`, `command_execute` must advertise the envelope command as
    // opaque, not the full union: every principal downloads it on every connect.
    let command_schema = tools
        .iter()
        .find(|tool| tool["name"] == "command_execute")
        .unwrap();
    assert!(
        command_schema["inputSchema"]["$defs"]["PrimitiveCommand"].is_null(),
        "{}",
        command_schema["inputSchema"]["$defs"]
    );
    assert!(
        command_schema["inputSchema"]["$defs"]["IntentCommand"].is_null(),
        "{}",
        command_schema["inputSchema"]["$defs"]
    );
    assert_eq!(
        command_schema["inputSchema"]["$defs"]["CommandEnvelope"]["properties"]["command"]["type"],
        "object"
    );
    // `CommandEnvelope` does not carry evidence, so the closure keeps `Evidence`
    // out of this tool entirely.
    assert!(
        command_schema["inputSchema"]["$defs"]["Evidence"].is_null(),
        "{}",
        command_schema["inputSchema"]["$defs"]
    );

    let checkpoint_schema = tools
        .iter()
        .find(|tool| tool["name"] == "checkpoint_save")
        .unwrap();
    assert_eq!(
        checkpoint_schema["inputSchema"]["$defs"]["WorkflowCheckpoint"]["properties"]
            ["recoveryReceipts"]["maxItems"],
        0
    );
    // `checkpoint_save` resolves evidence from `evidenceRefs` (command ids) against
    // the journal server-side; the caller never authors `Evidence` directly. Its
    // `evidence` and `recoveryHistory` fields are forced empty like
    // `recoveryReceipts` above, which keeps the `Evidence` union (and
    // `RecoveryDecision`/`RecoveryRecord`) out of this tool's reachable `$defs`.
    //
    // `workflow_recover`'s advertised output now projects `RecoveryDecision`
    // to status tags, so the tag-only `Evidence` projection no longer appears
    // in `tools/list` at all; it lives on the un-advertised output schema.
    let recover_schema = mcp_gateway::output_schema_for_test("workflow_recover");
    let evidence_variants = recover_schema["$defs"]["Evidence"]["oneOf"]
        .as_array()
        .unwrap();
    assert_eq!(evidence_variants.len(), 29, "{evidence_variants:?}");
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
    assert!(
        evidence_kinds.contains(&"formValidation"),
        "{evidence_kinds:?}"
    );
    assert!(
        evidence_kinds.contains(&"controlAction"),
        "{evidence_kinds:?}"
    );
    assert!(evidence_kinds.contains(&"emulation"), "{evidence_kinds:?}");
    assert!(evidence_kinds.contains(&"extraction"), "{evidence_kinds:?}");
    assert!(
        evidence_kinds.contains(&"submitSettlement"),
        "{evidence_kinds:?}"
    );

    assert_eq!(
        checkpoint_schema["inputSchema"]["$defs"]["WorkflowCheckpoint"]["properties"]["evidence"]
            ["maxItems"],
        0
    );
    assert_eq!(
        checkpoint_schema["inputSchema"]["$defs"]["WorkflowCheckpoint"]["properties"]
            ["recoveryHistory"]["maxItems"],
        0
    );
    assert!(
        checkpoint_schema["inputSchema"]["$defs"]["Evidence"].is_null(),
        "{}",
        checkpoint_schema["inputSchema"]["$defs"]
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
            expected_url: None,
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
    let oversized = vec![CommandId::new(); 129];
    let rejected = server
        .handle_message(request(
            52,
            "tools/call",
            json!({
                "name":"checkpoint_save","arguments":{"checkpoint":checkpoint,"evidenceRefs":oversized}
            }),
        ))
        .await
        .unwrap();
    // The violation payload is pinned, not just the top-level `-32602`: this
    // fixture's `RuntimeService::default()` has no journal, so an in-bounds
    // `evidenceRefs` also fails, just with a different error shape.
    assert_eq!(rejected["error"]["code"], -32602, "{rejected}");
    assert_eq!(
        rejected["error"]["data"]["pointer"], "/evidenceRefs",
        "{rejected}"
    );
    assert_eq!(
        rejected["error"]["data"]["constraint"], "maxItems",
        "{rejected}"
    );
    assert_eq!(runtime.checkpoint_dispatch_count(), 0);
}

// `session_create`'s MCP schema exposes an optional `executionPolicy` object mapping
// into `CreateSessionRequest.execution_policy`. Both sides of deny-by-default: an
// explicit grant is honored, an omitted field falls back to `ExecutionPolicy::default()`.

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

#[tokio::test]
async fn session_create_rejects_a_named_vision_node_when_runtime_has_no_provider() {
    let server = fixture_server(vec![Capability::SessionWrite]).await;
    let response = server
        .handle_message(request(
            62,
            "tools/call",
            json!({
                "name":"session_create",
                "arguments":{
                    "profile":"fixture",
                    "executionPolicy":{
                        "visionAssist":true,
                        "visionNode":"acp-codex"
                    }
                }
            }),
        ))
        .await
        .unwrap();

    assert_eq!(
        response["error"]["data"]["interfaceError"]["code"], "invalidRequest",
        "{response}"
    );
}

// `checkpoint_save` names a command via `evidenceRefs` and the server resolves the
// evidence from the journal. Guards that a real journaled
// `javaScriptResult`/`accessibilitySnapshot` outcome resolves by id and reaches
// dispatch.
#[tokio::test]
async fn checkpoint_save_resolves_javascript_and_actionable_accessibility_evidence_by_ref() {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000021")),
            [
                Capability::RecoveryWrite,
                Capability::RecoveryRead,
                Capability::SessionWrite,
                Capability::PageWrite,
                Capability::BrowserMutate,
            ],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let live = common::live_server(handle).await;
    common::initialize(&live.server).await;
    let mut next_id = 700;
    let (session_id, page_id) = common::create_session_and_page(&live.server, &mut next_id).await;

    let workflow_id = WorkflowId::new();
    let attempt_id = AttemptId::new();
    let command_id = CommandId::new();
    let evidence = vec![
        Evidence::JavaScriptResult {
            value: json!({"answer": 42}),
            truncated: false,
        },
        Evidence::AccessibilitySnapshot {
            page_id: page_id.clone(),
            nodes: vec![types::AccessibilityNode {
                role: Some("textbox".into()),
                name: Some("Email".into()),
                target: Some(types::AccessibilityTarget {
                    role: "textbox".into(),
                    accessible_name: "Email".into(),
                    ordinal: Some(1),
                    frame_path: Vec::new(),
                }),
                ..types::AccessibilityNode::default()
            }],
            truncated: false,
        },
    ];
    // The `Accepted` phase record carries the envelope (and so the owning session),
    // the terminal one carries the outcome. `resolve_command_evidence` needs both:
    // the envelope to verify ownership, the outcome for the evidence.
    live.journal
        .append(JournalRecord {
            sequence: 0,
            recorded_at: Utc::now(),
            command_id: command_id.clone(),
            phase: types::CommandPhase::Accepted,
            envelope: Some(CommandEnvelope {
                schema_version: CommandEnvelope::SCHEMA_VERSION,
                command_id: command_id.clone(),
                workflow_id: workflow_id.clone(),
                attempt_id: attempt_id.clone(),
                session_id: session_id.clone(),
                page_id: Some(page_id.clone()),
                deadline: Utc::now() + Duration::seconds(30),
                command: RuntimeCommand::Primitive(PrimitiveCommand::Inspect(
                    types::InspectCommand::default(),
                )),
            }),
            outcome: None,
            prepared_result: None,
        })
        .await
        .unwrap();
    live.journal
        .append(JournalRecord {
            sequence: 0,
            recorded_at: Utc::now(),
            command_id: command_id.clone(),
            phase: types::CommandPhase::Completed,
            envelope: None,
            outcome: Some(types::CommandOutcome::Completed {
                command_id: command_id.clone(),
                evidence,
            }),
            prepared_result: None,
        })
        .await
        .unwrap();

    let checkpoint = WorkflowCheckpoint {
        schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
        checkpoint_id: CheckpointId::new(),
        workflow_id,
        attempt_id,
        session_id,
        page_id,
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

    let saved = live
        .server
        .handle_message(request(
            54,
            "tools/call",
            json!({
                "name":"checkpoint_save",
                "arguments":{"checkpoint":checkpoint,"evidenceRefs":[command_id]}
            }),
        ))
        .await
        .unwrap();
    assert!(saved["error"].is_null(), "{saved}");

    // Read it back: the persisted checkpoint must carry the evidence the
    // runtime resolved from the journal, not an empty bag.
    let status = live
        .server
        .handle_message(request(
            55,
            "tools/call",
            json!({
                "name":"recovery_status",
                "arguments":{"workflowId":checkpoint.workflow_id.0.to_string()}
            }),
        ))
        .await
        .unwrap();
    let kinds: Vec<&str> = status["result"]["structuredContent"]["checkpoint"]["evidence"]
        .as_array()
        .unwrap_or_else(|| panic!("checkpoint has no evidence: {status}"))
        .iter()
        .filter_map(|item| item["kind"].as_str())
        .collect();
    assert!(
        kinds.contains(&"javaScriptResult") && kinds.contains(&"accessibilitySnapshot"),
        "resolved evidence must land in the checkpoint: {status}"
    );
}

/// A worker that succeeds, so a real `command_execute` for one principal lands
/// evidence in the shared journal.
struct MinimalWorker {
    profile: std::path::PathBuf,
}

#[async_trait]
impl BrowserWorker for MinimalWorker {
    fn worker_id(&self) -> types::WorkerId {
        types::WorkerId::new()
    }
    fn profile_dir(&self) -> &std::path::Path {
        &self.profile
    }
    async fn open_page(&self, _: types::PageId) -> Result<(), types::CommandError> {
        Ok(())
    }
    async fn navigate(
        &self,
        _: &types::PageId,
        _: &types::NavigateCommand,
    ) -> Result<Vec<Evidence>, types::CommandError> {
        Ok(Vec::new())
    }
    async fn inspect(
        &self,
        _: &types::PageId,
        _: &types::InspectCommand,
    ) -> Result<Vec<Evidence>, types::CommandError> {
        Ok(Vec::new())
    }
    async fn click(
        &self,
        _: &types::PageId,
        _: &types::ClickCommand,
    ) -> Result<Vec<Evidence>, types::CommandError> {
        Ok(Vec::new())
    }
    async fn type_text(
        &self,
        _: &types::PageId,
        _: &types::TypeTextCommand,
    ) -> Result<Vec<Evidence>, types::CommandError> {
        Ok(Vec::new())
    }
    // Overridden because the trait default is `Err(unsupported)`. A `ListPages`
    // outcome must carry `Evidence::Pages` to verify as completed, so the marker
    // string lives in that variant's `url`/`title` fields.
    async fn list_pages(
        &self,
        _: &types::ListPagesCommand,
    ) -> Result<Vec<Evidence>, types::CommandError> {
        Ok(vec![Evidence::Pages {
            pages: vec![types::PageEvidence {
                page_id: types::PageId::new(),
                url: "https://victim.example.test/classified-only-b-should-ever-read-this"
                    .to_owned(),
                title: "principal-b-secret".to_owned(),
            }],
        }])
    }
    async fn close(&self) -> Result<(), types::CommandError> {
        Ok(())
    }
}

struct MinimalFactory;

#[async_trait]
impl WorkerFactory for MinimalFactory {
    async fn launch(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<dyn BrowserWorker>, types::CommandError> {
        Ok(Arc::new(MinimalWorker {
            profile: std::path::PathBuf::from(format!("/profiles/{}", session_id.0)),
        }))
    }
}

fn session_id_from_structured_content(response: &Value) -> SessionId {
    SessionId(
        uuid::Uuid::parse_str(
            response["result"]["structuredContent"]["id"]
                .as_str()
                .unwrap(),
        )
        .unwrap(),
    )
}

// SECURITY: the journal `evidenceRefs` resolves against is one `PageRuntime` shared
// by every authenticated principal, so a command id is not proof of ownership.
// Principal A naming principal B's command must be rejected, and B's evidence must
// never appear anywhere in A's response.
#[tokio::test]
async fn checkpoint_save_rejects_evidence_refs_for_a_command_another_principal_owns() {
    let root = tempfile::tempdir().unwrap();
    let journal = Arc::new(
        JsonlJournal::open(root.path().join("journal.jsonl"))
            .await
            .unwrap(),
    );
    let workers = Arc::new(WorkerPool::new(4, Arc::new(MinimalFactory)));
    // A real `RecoveryCoordinator` is required here: without one,
    // `checkpoint_with_evidence` fails with `RecoveryError::WorkersUnavailable`
    // regardless of what resolved, so the test would pass with the ownership
    // check removed.
    let checkpoint_store = checkpoint_store::CheckpointStore::open(root.path().join("checkpoints"))
        .await
        .unwrap();
    let recovery = page_runtime::RecoveryCoordinator::new(checkpoint_store);
    let runtime_service = RuntimeService::with_recovery(
        SessionManager::new(workers.clone()),
        PageRuntime::new(journal, workers),
        recovery,
    );

    let authority = AuthorityStore::with_capacity(2);
    let (_ownership, recorder) = interface_core::SessionOwnershipRegistry::bounded(4);
    let capabilities = [
        Capability::SessionWrite,
        Capability::PageWrite,
        Capability::BrowserMutate,
        Capability::RecoveryWrite,
    ];

    let token_a = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-0000000000a1")),
            capabilities,
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle_a = authority.verify(&token_a.expose_once()).await.unwrap();
    let server_a = Server::new(Arc::new(AuthenticatedRuntime::with_session_ownership(
        runtime_service.clone(),
        handle_a,
        recorder.clone(),
    )));
    initialize(&server_a).await;

    let token_b = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-0000000000b2")),
            capabilities,
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle_b = authority.verify(&token_b.expose_once()).await.unwrap();
    let server_b = Server::new(Arc::new(AuthenticatedRuntime::with_session_ownership(
        runtime_service,
        handle_b,
        recorder,
    )));
    initialize(&server_b).await;

    // B creates its own session and runs a command whose evidence only B
    // should ever be able to read.
    let session_b = server_b
        .handle_message(request(
            1,
            "tools/call",
            json!({"name":"session_create","arguments":{"profile":"victim"}}),
        ))
        .await
        .unwrap();
    let session_b_id = session_id_from_structured_content(&session_b);

    let secret_command_id = CommandId::new();
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: secret_command_id.clone(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: session_b_id,
        page_id: None,
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Primitive(PrimitiveCommand::ListPages(types::ListPagesCommand)),
    };
    let submitted = server_b
        .handle_message(request(
            2,
            "tools/call",
            json!({"name":"command_execute","arguments":{"envelope":envelope}}),
        ))
        .await
        .unwrap();
    assert_eq!(
        submitted["result"]["structuredContent"]["status"], "completed",
        "{submitted}"
    );

    // A creates its own, unrelated session, then tries to checkpoint by
    // naming B's command as evidenceRefs.
    let session_a = server_a
        .handle_message(request(
            3,
            "tools/call",
            json!({"name":"session_create","arguments":{"profile":"attacker"}}),
        ))
        .await
        .unwrap();
    let session_a_id = session_id_from_structured_content(&session_a);

    let checkpoint = WorkflowCheckpoint {
        schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
        checkpoint_id: CheckpointId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: session_a_id,
        page_id: types::PageId::new(),
        restart_url: "https://attacker.example.test/".to_owned(),
        current_url: "https://attacker.example.test/".to_owned(),
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
    let response = server_a
        .handle_message(request(
            4,
            "tools/call",
            json!({
                "name":"checkpoint_save",
                "arguments":{"checkpoint":checkpoint,"evidenceRefs":[secret_command_id]}
            }),
        ))
        .await
        .unwrap();

    assert!(
        response.get("result").is_none(),
        "principal A must not be able to checkpoint using principal B's command: {response}"
    );
    assert_ne!(
        response["error"]["code"], -32602,
        "must be rejected as an ownership/authorization failure, not a schema violation: {response}"
    );
    let body = response.to_string();
    assert!(
        !body.contains("principal-b-secret") && !body.contains("classified"),
        "principal B's evidence must never appear in principal A's response: {response}"
    );
}

#[tokio::test]
async fn command_execute_schema_accepts_locate_intent_envelope() {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000022")),
            [
                Capability::BrowserMutate,
                Capability::IntentExecute,
                Capability::SessionWrite,
                Capability::PageWrite,
            ],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let live = common::live_server(handle).await;
    common::initialize(&live.server).await;
    let mut next_id = 100;
    let (session_id, page_id) = common::create_session_and_page(&live.server, &mut next_id).await;

    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id,
        page_id: Some(page_id),
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Intent(IntentCommand::Locate(LocateIntent {
            purpose: "Continue".to_owned(),
            hints: IntentHints::default(),
        })),
    };

    let response = live
        .server
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

    // The envelope must execute to a terminal outcome through the real
    // runtime, not merely pass schema validation. The fake DOM has no
    // candidates, so the intent engine's own evidence proves it ran:
    // deterministic resolution, zero candidates, verification targetNotFound.
    let content = &response["result"]["structuredContent"];
    assert!(response["error"].is_null(), "{response}");
    assert_eq!(content["commandId"], envelope.command_id.0.to_string());
    assert_eq!(content["status"], "failed", "{response}");
    assert_eq!(response["result"]["isError"], json!(true), "{response}");
    let record = &content["evidence"][0]["record"];
    assert_eq!(record["intentKind"], "locate", "{response}");
    assert_eq!(record["resolutionPath"], "deterministic", "{response}");
    assert_eq!(record["candidates"], json!([]), "{response}");
    assert_eq!(record["verification"], "targetNotFound", "{response}");
    // Command-layer failures carry the machine-readable repair hint on the
    // error itself; the fake DOM denies vision assist, whose message leads
    // with the stuck reason and whose repair is to fix that reason first.
    assert_eq!(
        content["error"]["code"],
        json!("visionAssistDenied"),
        "{response}"
    );
    assert!(
        content["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("targetNotFound"),
        "{response}"
    );
    assert!(
        content["error"]["repair"]["action"]
            .as_str()
            .unwrap()
            .contains("stuck reason"),
        "{response}"
    );
    assert_eq!(
        content["error"]["repair"]["doc"],
        json!("bobby://failure-taxonomy")
    );
}

#[tokio::test]
async fn command_execute_schema_accepts_fill_intent_with_snapshot_ordinal() {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000071")),
            [
                Capability::BrowserMutate,
                Capability::IntentExecute,
                Capability::SessionWrite,
                Capability::PageWrite,
            ],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let live = common::live_server(handle).await;
    common::initialize(&live.server).await;
    let mut next_id = 200;
    let (session_id, page_id) = common::create_session_and_page(&live.server, &mut next_id).await;

    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id,
        page_id: Some(page_id),
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Intent(IntentCommand::Fill(FillIntent {
            purpose: "enter the applicant name".into(),
            hints: IntentHints {
                role: Some("textbox".into()),
                near_text: Some(TextMatch::Exact("Name".into())),
                ..IntentHints::default()
            },
            value: ControlAction::SetText {
                value: "Ada".into(),
                clear_first: true,
            },
        })),
    };
    // The snapshot-produced hints shape (ordinal present) must parse AND run.
    let mut envelope_value = serde_json::to_value(&envelope).unwrap();
    envelope_value["command"]["input"]["input"]["hints"]["ordinal"] = json!(1);
    next_id += 1;
    let response = live
        .server
        .handle_message(request(
            next_id,
            "tools/call",
            json!({
                "name":"command_execute", "arguments":{"envelope":envelope_value}
            }),
        ))
        .await
        .unwrap();
    common::assert_intent_domain_failure(&response, &envelope, "fill");
}

#[tokio::test]
async fn command_execute_schema_accepts_bounded_complete_form_intent() {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000072")),
            [
                Capability::BrowserMutate,
                Capability::IntentExecute,
                Capability::SessionWrite,
                Capability::PageWrite,
            ],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let live = common::live_server(handle).await;
    common::initialize(&live.server).await;
    let mut next_id = 300;
    let (session_id, page_id) = common::create_session_and_page(&live.server, &mut next_id).await;

    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id,
        page_id: Some(page_id),
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Intent(IntentCommand::CompleteForm(CompleteFormIntent {
            purpose: "complete application form".into(),
            fields: vec![CompleteFormField {
                name: "email".into(),
                purpose: "enter email".into(),
                hints: IntentHints {
                    role: Some("textbox".into()),
                    near_text: Some(TextMatch::Exact("Email address".into())),
                    ..IntentHints::default()
                },
                value: ControlAction::SetText {
                    value: "ada@example.test".into(),
                    clear_first: true,
                },
            }],
        })),
    };
    let response = common::execute_envelope(&live.server, &mut next_id, &envelope).await;
    // completeForm decomposes into per-field fill intents, and the harness
    // DOM has no candidates, so the first field fails deterministically.
    common::assert_intent_domain_failure(&response, &envelope, "fill");
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
            [
                Capability::BrowserMutate,
                Capability::IntentExecute,
                Capability::SessionWrite,
                Capability::PageWrite,
            ],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let live = common::live_server(handle).await;
    common::initialize(&live.server).await;
    let mut next_id = 400;
    let (session_id, page_id) = common::create_session_and_page(&live.server, &mut next_id).await;

    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id,
        page_id: Some(page_id),
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

    let response = common::execute_envelope(&live.server, &mut next_id, &envelope).await;
    common::assert_intent_domain_failure(&response, &envelope, "follow");
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
            [
                Capability::BrowserMutate,
                Capability::IntentExecute,
                Capability::SessionWrite,
                Capability::PageWrite,
            ],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let live = common::live_server(handle).await;
    common::initialize(&live.server).await;
    let mut next_id = 500;
    let (session_id, page_id) = common::create_session_and_page(&live.server, &mut next_id).await;

    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id,
        page_id: Some(page_id),
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Intent(IntentCommand::DismissObstruction(
            DismissObstructionIntent {
                purpose: "Cookie notice close button".to_owned(),
                hints: IntentHints::default(),
                timeout_ms: 5_000,
            },
        )),
    };

    let response = common::execute_envelope(&live.server, &mut next_id, &envelope).await;
    common::assert_intent_domain_failure(&response, &envelope, "dismissObstruction");
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
            [
                Capability::BrowserMutate,
                Capability::IntentExecute,
                Capability::SessionWrite,
                Capability::PageWrite,
            ],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let live = common::live_server(handle).await;
    common::initialize(&live.server).await;
    let mut next_id = 600;
    let (session_id, page_id) = common::create_session_and_page(&live.server, &mut next_id).await;

    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id,
        page_id: Some(page_id),
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

    let response = common::execute_envelope(&live.server, &mut next_id, &envelope).await;
    // extract is Replayable: unresolvable fields are reported per-field
    // rather than failing the call. The harness DOM has no candidates, so
    // both fields come back deterministic-but-missing.
    let content = &response["result"]["structuredContent"];
    assert!(response["error"].is_null(), "{response}");
    assert_eq!(content["status"], "completed", "{response}");
    assert_eq!(response["result"]["isError"], json!(false), "{response}");
    let fields: Vec<&str> = content["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["kind"] == "extraction")
        .filter_map(|item| item["field"].as_str())
        .collect();
    assert_eq!(fields, ["displayName", "profileLink"], "{response}");
    let record = &content["evidence"][2]["record"];
    assert_eq!(record["intentKind"], "extract", "{response}");
    assert_eq!(record["resolutionPath"], "deterministic", "{response}");
    assert_eq!(
        record["verification"], "extractedPartial:missing=displayName,profileLink",
        "{response}"
    );
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
    assert_eq!(navigate["inputSchema"]["required"], json!(["url"]));
    assert!(navigate["inputSchema"]["oneOf"]
        .as_array()
        .expect("navigate scope branches")
        .iter()
        .any(|branch| branch["required"] == json!(["sessionId", "pageId", "workflowId"])));
    let click = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "click")
        .unwrap();
    assert!(
        click["inputSchema"]["oneOf"]
            .as_array()
            .expect("click scope branches")
            .iter()
            .any(|branch| branch["required"] == json!(["sessionId", "pageId", "workflowId"])),
        "semantic snapshot targets must not require a legacy CSS selector"
    );
    let type_text = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "type_text")
        .unwrap();
    assert_eq!(type_text["inputSchema"]["required"], json!(["value"]));
    assert!(type_text["inputSchema"]["oneOf"]
        .as_array()
        .expect("type_text scope branches")
        .iter()
        .any(|branch| branch["required"] == json!(["sessionId", "pageId", "workflowId"])));
    let upload_files = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "upload_files")
        .unwrap();
    assert_eq!(upload_files["inputSchema"]["required"], json!(["paths"]));
    assert!(
        upload_files["inputSchema"]["oneOf"]
            .as_array()
            .expect("upload_files scope branches")
            .iter()
            .any(|branch| branch["required"] == json!(["sessionId", "pageId", "workflowId"])),
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
    for hidden in [
        "download_url",
        "upload_files",
        "evaluate_javascript",
        "extract_structured",
    ] {
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
async fn form_snapshot_is_a_read_only_page_tool() {
    let page_reader = fixture_server(vec![Capability::PageRead]).await;
    let listed = page_reader
        .handle_message(request(73, "tools/list", json!({})))
        .await
        .unwrap();
    let tools = listed["result"]["tools"].as_array().unwrap();
    let snapshot = tools
        .iter()
        .find(|tool| tool["name"] == "form_snapshot")
        .expect("page readers can discover form_snapshot");
    assert_eq!(snapshot["inputSchema"]["required"], json!([]));
    assert!(snapshot["inputSchema"]["oneOf"]
        .as_array()
        .expect("form_snapshot scope branches")
        .iter()
        .any(|branch| branch["required"] == json!(["sessionId", "pageId"])));
    // Sorted before comparing: `serde_json::Map` iterates sorted by default and in
    // insertion order under `preserve_order`, which any crate in the graph can
    // enable. The assertion is about which properties exist, not their order.
    let mut properties = snapshot["inputSchema"]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    properties.sort();
    assert_eq!(
        properties,
        vec![
            "maxControls",
            "pageId",
            "sessionId",
            "workflowHandle",
            "workflowId"
        ]
    );
    assert_eq!(
        snapshot["inputSchema"]["properties"]["maxControls"],
        json!({"type":"integer","minimum":1,"maximum":512})
    );

    let mutate_only = fixture_server(vec![Capability::BrowserMutate]).await;
    let listed = mutate_only
        .handle_message(request(74, "tools/list", json!({})))
        .await
        .unwrap();
    assert!(
        listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool["name"] != "form_snapshot"),
        "browser mutation authority must not imply page read authority"
    );
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

    let modified_click = server
        .handle_message(request(
            811,
            "tools/call",
            json!({
                "name":"click",
                "arguments":{
                    "sessionId":SessionId::new().0.to_string(),
                    "pageId":types::PageId::new().0.to_string(),
                    "selector":"#range-end",
                    "modifiers":["shift"]
                }
            }),
        ))
        .await
        .unwrap();
    assert!(modified_click["error"].is_null(), "{modified_click}");
    assert_eq!(
        modified_click["result"]["structuredContent"]["status"],
        json!("failed"),
        "modifier arguments must reach runtime dispatch instead of failing MCP parsing"
    );

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
    // A failed command is a failed tool call: isError mirrors the outcome
    // status rather than reporting transport-level success.
    assert_eq!(navigated["result"]["isError"], json!(true));
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

#[tokio::test]
async fn tools_list_fits_the_frame_budget_for_a_full_capability_principal() {
    // A `tools/list` result past MAX_FRAME_BYTES answers `resultTooLarge`, which
    // leaves no way to enumerate the surface.
    let server = fixture_server(vec![
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
    ])
    .await;

    let list = server
        .handle_message(request(90, "tools/list", json!({})))
        .await
        .expect("tools/list returns a response");
    assert!(
        list.get("error").is_none(),
        "tools/list must not exceed the frame budget: {list}"
    );

    let mut sizes = list["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|tool| {
            (
                serde_json::to_vec(tool).expect("serializable").len(),
                tool["name"].as_str().expect("tool name").to_owned(),
            )
        })
        .collect::<Vec<_>>();
    sizes.sort_unstable();
    sizes.reverse();
    let breakdown = sizes
        .iter()
        .map(|(size, name)| format!("{name}={size}"))
        .collect::<Vec<_>>()
        .join(" ");

    let bytes = serde_json::to_vec(&list).expect("serializable").len();
    assert!(
        bytes <= TOOLS_LIST_BYTE_BUDGET,
        "tools/list is {bytes} bytes, budget is {TOOLS_LIST_BYTE_BUDGET}: {breakdown}"
    );

    // No single tool may reintroduce the shared type system either.
    for (size, name) in &sizes {
        assert!(
            *size <= PER_TOOL_BYTE_BUDGET,
            "tool {name} schema is {size} bytes, budget is {PER_TOOL_BYTE_BUDGET}"
        );
    }
}

use mcp_gateway::{PER_TOOL_BYTE_BUDGET, TOOLS_LIST_BYTE_BUDGET};

async fn authenticated_with_intents() -> AuthenticatedRuntime {
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000031")),
            [Capability::BrowserMutate, Capability::IntentExecute],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    AuthenticatedRuntime::new(RuntimeService::default(), handle)
}

const INTENT_TOOLS: [&str; 8] = [
    "intent_complete_form",
    "intent_dismiss_obstruction",
    "intent_extract",
    "intent_fill",
    "intent_follow",
    "intent_locate",
    "intent_submit_and_verify",
    "intent_wait_for_state",
];

#[tokio::test]
async fn intent_tools_require_intent_execute_alongside_browser_mutate() {
    // `browser:mutate` alone reaches the primitives but not the semantic layer.
    let server = Server::new(Arc::new(authenticated_with_browser_mutate().await))
        .with_startup_toolset(mcp_gateway::Toolset::Full);
    initialize(&server).await;
    let listed = server
        .handle_message(request(70, "tools/list", json!({})))
        .await
        .unwrap();
    let names = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    for tool in INTENT_TOOLS {
        assert!(
            !names.contains(&tool),
            "{tool} advertised without intent:execute"
        );
    }

    // An unadvertised tool is not merely hidden: calling it fails without dispatch.
    let runtime = Arc::new(authenticated_with_browser_mutate().await);
    let server = Server::new(runtime.clone());
    initialize(&server).await;
    let denied = server
        .handle_message(request(
            71,
            "tools/call",
            json!({
                "name":"intent_locate",
                "arguments":{
                    "sessionId":SessionId::new().0.to_string(),
                    "pageId":types::PageId::new().0.to_string(),
                    "purpose":"the search box"
                }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(denied["error"]["code"], -32601, "{denied}");
    assert_eq!(runtime.submit_dispatch_count(), 0);

    let server = Server::new(Arc::new(authenticated_with_intents().await))
        .with_startup_toolset(mcp_gateway::Toolset::Full);
    initialize(&server).await;
    let listed = server
        .handle_message(request(72, "tools/list", json!({})))
        .await
        .unwrap();
    let names = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    for tool in INTENT_TOOLS {
        assert!(names.contains(&tool), "{tool} missing: {names:?}");
    }
}

#[tokio::test]
async fn intent_tools_build_their_own_envelope_and_thread_the_workflow() {
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000073")),
            [
                Capability::BrowserMutate,
                Capability::IntentExecute,
                Capability::SessionWrite,
                Capability::PageWrite,
            ],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let live = common::live_server(handle).await;
    common::initialize(&live.server).await;
    let mut next_id = 900;
    let (session, page) = common::create_session_and_page(&live.server, &mut next_id).await;
    let session_id = session.0.to_string();
    let page_id = page.0.to_string();

    // No commandId/workflowId/attemptId/deadline from the caller.
    let located = live
        .server
        .handle_message(request(
            73,
            "tools/call",
            json!({
                "name":"intent_locate",
                "arguments":{"sessionId":session_id,"pageId":page_id,"purpose":"the search box"}
            }),
        ))
        .await
        .unwrap();
    assert!(located["error"].is_null(), "{located}");
    assert!(
        located["result"]["structuredContent"]["commandId"].is_string(),
        "{located}"
    );
    // The outcome is real too: the intent engine ran and found no candidates
    // on the fake DOM.
    assert_eq!(
        located["result"]["structuredContent"]["status"], "failed",
        "{located}"
    );
    assert_eq!(
        located["result"]["structuredContent"]["evidence"][0]["record"]["verification"],
        "targetNotFound",
        "{located}"
    );
    let minted = located["result"]["structuredContent"]["workflowId"]
        .as_str()
        .expect("outcome names its workflow");

    // Passing that workflow back keeps the next intent in the same workflow,
    // which is what makes `checkpoint_save` reachable from these tools.
    let filled = live
        .server
        .handle_message(request(
            74,
            "tools/call",
            json!({
                "name":"intent_fill",
                "arguments":{
                    "sessionId":session_id,
                    "pageId":page_id,
                    "workflowId":minted,
                    "purpose":"enter the applicant email",
                    "hints":{"role":"textbox"},
                    "value":{"kind":"setText","value":"a@example.test","clearFirst":true}
                }
            }),
        ))
        .await
        .unwrap();
    assert!(filled["error"].is_null(), "{filled}");
    assert_eq!(
        filled["result"]["structuredContent"]["workflowId"],
        json!(minted),
        "{filled}"
    );

    // Omitting it mints a fresh workflow instead of reusing the last one.
    let separate = live
        .server
        .handle_message(request(
            75,
            "tools/call",
            json!({
                "name":"intent_locate",
                "arguments":{"sessionId":session_id,"pageId":page_id,"purpose":"the login link"}
            }),
        ))
        .await
        .unwrap();
    assert_ne!(
        separate["result"]["structuredContent"]["workflowId"],
        json!(minted),
        "{separate}"
    );

    // Arguments stay closed.
    let unknown = live
        .server
        .handle_message(request(
            76,
            "tools/call",
            json!({
                "name":"intent_extract",
                "arguments":{
                    "sessionId":session_id,
                    "pageId":page_id,
                    "purpose":"product fields",
                    "fields":[{"name":"title","purpose":"product title","value":{"kind":"text"}}],
                    "surprise":true
                }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(unknown["error"]["code"], -32602, "{unknown}");

    // `intent_wait_for_state` carries no purpose of its own.
    let waited = live.server
        .handle_message(request(77, "tools/call", json!({
            "name":"intent_wait_for_state",
            "arguments":{
                "sessionId":session_id,
                "pageId":page_id,
                "condition":{"kind":"url","matcher":{"kind":"exact","value":"https://example.test/done"}},
                "timeoutMs":5000
            }
        })))
        .await
        .unwrap();
    assert!(waited["error"].is_null(), "{waited}");
}

#[tokio::test]
async fn rejected_arguments_name_the_offending_field_and_constraint() {
    // A rejection must name the offending field in `data`; that is the only signal
    // that lets a caller repair the call instead of guessing against the schema.
    let server = fixture_server(vec![Capability::SessionWrite]).await;

    let missing = server
        .handle_message(request(
            60,
            "tools/call",
            json!({"name":"session_create","arguments":{}}),
        ))
        .await
        .unwrap();
    assert_eq!(missing["error"]["code"], -32602, "{missing}");
    assert_eq!(missing["error"]["data"]["reason"], json!("schemaViolation"));
    assert_eq!(missing["error"]["data"]["pointer"], json!("/profile"));
    assert_eq!(missing["error"]["data"]["constraint"], json!("required"));

    let unknown = server
        .handle_message(request(
            61,
            "tools/call",
            json!({"name":"session_create","arguments":{"profile":"p","bearer":"nope"}}),
        ))
        .await
        .unwrap();
    assert_eq!(
        unknown["error"]["data"]["pointer"],
        json!("/bearer"),
        "{unknown}"
    );
    assert_eq!(
        unknown["error"]["data"]["constraint"],
        json!("additionalProperties")
    );

    let too_long = server
        .handle_message(request(
            62,
            "tools/call",
            json!({"name":"session_create","arguments":{"profile":"p".repeat(129)}}),
        ))
        .await
        .unwrap();
    assert_eq!(
        too_long["error"]["data"]["pointer"],
        json!("/profile"),
        "{too_long}"
    );
    assert_eq!(too_long["error"]["data"]["constraint"], json!("maxLength"));

    // The pointer walks into arrays and nested objects, not just top-level keys.
    let server = fixture_server(vec![Capability::BrowserMutate, Capability::IntentExecute]).await;
    let nested = server
        .handle_message(request(
            63,
            "tools/call",
            json!({
                "name":"intent_extract",
                "arguments":{
                    "sessionId":SessionId::new().0.to_string(),
                    "pageId":types::PageId::new().0.to_string(),
                    "purpose":"product fields",
                    "fields":[
                        {"name":"title","purpose":"product title","value":{"kind":"text"}},
                        {"name":"link","purpose":"product link"}
                    ]
                }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(
        nested["error"]["data"]["pointer"],
        json!("/fields/1/value"),
        "{nested}"
    );
    assert_eq!(nested["error"]["data"]["constraint"], json!("required"));

    // A body that clears the schema but fails to deserialize is reported as its
    // own reason rather than a field-level violation.
    let server = fixture_server(vec![Capability::BrowserMutate]).await;
    let stale_deadline = server
        .handle_message(request(
            64,
            "tools/call",
            json!({
                "name":"command_execute",
                "arguments":{"envelope":{
                    "schemaVersion":2,
                    "commandId":CommandId::new().0.to_string(),
                    "workflowId":WorkflowId::new().0.to_string(),
                    "attemptId":AttemptId::new().0.to_string(),
                    "sessionId":SessionId::new().0.to_string(),
                    "deadline":"2020-01-01T00:00:00Z",
                    "command":{"kind":"primitive","input":{"kind":"listPages","input":null}}
                }}
            }),
        ))
        .await
        .unwrap();
    assert_eq!(stale_deadline["error"]["code"], -32602, "{stale_deadline}");
    assert_eq!(
        stale_deadline["error"]["data"]["reason"],
        json!("deadlineOutOfRange"),
        "{stale_deadline}"
    );
}

#[tokio::test]
async fn legacy_fill_value_shape_gets_a_migration_repair_hint() {
    // A caller still on the pre-0.11.0 FillValue shape gets the exact
    // kind/field rename, not just a bare `oneOf` mismatch.
    let server = fixture_server(vec![Capability::BrowserMutate, Capability::IntentExecute]).await;
    let session_id = SessionId::new().0.to_string();
    let page_id = PageId::new().0.to_string();

    let rejected = server
        .handle_message(request(
            65,
            "tools/call",
            json!({
                "name":"intent_fill",
                "arguments":{
                    "sessionId":session_id,
                    "pageId":page_id,
                    "purpose":"Email",
                    "value":{"kind":"text","text":"a@b.co"}
                }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(
        rejected["error"]["data"]["reason"],
        json!("schemaViolation"),
        "{rejected}"
    );
    let action = rejected["error"]["data"]["repair"]["action"]
        .as_str()
        .expect("repair action");
    assert!(action.contains("setText"), "{action}");
    assert!(action.contains("0.11.0"), "{action}");

    // A payload with no legacy marker gets the plain schemaViolation repair,
    // unchanged.
    let plain = server
        .handle_message(request(
            66,
            "tools/call",
            json!({
                "name":"intent_fill",
                "arguments":{
                    "sessionId":session_id,
                    "pageId":page_id,
                    "purpose":"Email",
                    "value":{"kind":"bogus"}
                }
            }),
        ))
        .await
        .unwrap();
    let plain_action = plain["error"]["data"]["repair"]["action"]
        .as_str()
        .expect("repair action");
    assert!(!plain_action.contains("0.11.0"), "{plain_action}");
}

#[tokio::test]
async fn command_execute_accepts_an_agent_authored_deadline_within_five_minutes() {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000065")),
            [
                Capability::BrowserMutate,
                Capability::SessionWrite,
                Capability::PageWrite,
            ],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let live = common::live_server(handle).await;
    common::initialize(&live.server).await;
    let mut next_id = 800;
    let (session_id, _) = common::create_session_and_page(&live.server, &mut next_id).await;
    let deadline = (Utc::now() + Duration::seconds(270)).to_rfc3339();

    let response = live
        .server
        .handle_message(request(
            65,
            "tools/call",
            json!({
                "name":"command_execute",
                "arguments":{"envelope":{
                    "schemaVersion":2,
                    "commandId":"10000000-0000-0000-0000-000000000101",
                    "workflowId":"10000000-0000-0000-0000-000000000102",
                    "attemptId":"10000000-0000-0000-0000-000000000103",
                    "sessionId":session_id.0.to_string(),
                    "pageId":null,
                    "deadline":deadline,
                    "command":{"kind":"primitive","input":{"kind":"listPages","input":null}}
                }}
            }),
        ))
        .await
        .unwrap();

    // The deadline is accepted AND the command completes through the real
    // runtime: listPages is exempt from the pageId requirement, and the fake
    // worker returns Pages evidence that verifies.
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(
        content["commandId"], "10000000-0000-0000-0000-000000000101",
        "{response}"
    );
    assert_eq!(content["status"], "completed", "{response}");
    assert_eq!(content["evidence"][0]["kind"], "pages", "{response}");
}

#[tokio::test]
async fn command_execute_rejects_an_agent_deadline_beyond_five_minutes() {
    let server = fixture_server(vec![Capability::BrowserMutate]).await;
    let deadline = (Utc::now() + Duration::seconds(310)).to_rfc3339();

    let response = server
        .handle_message(request(
            66,
            "tools/call",
            json!({
                "name":"command_execute",
                "arguments":{"envelope":{
                    "schemaVersion":2,
                    "commandId":"10000000-0000-0000-0000-000000000111",
                    "workflowId":"10000000-0000-0000-0000-000000000112",
                    "attemptId":"10000000-0000-0000-0000-000000000113",
                    "sessionId":"10000000-0000-0000-0000-000000000114",
                    "pageId":null,
                    "deadline":deadline,
                    "command":{"kind":"primitive","input":{"kind":"listPages","input":null}}
                }}
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response["error"]["code"], json!(-32602), "{response}");
    assert_eq!(
        response["error"]["data"]["reason"],
        json!("deadlineOutOfRange"),
        "{response}"
    );
}

#[tokio::test]
async fn recovery_status_follows_recovery_read_capability() {
    let server = fixture_server(vec![Capability::RecoveryRead]).await;
    let listed = server
        .handle_message(request(95, "tools/list", json!({})))
        .await
        .unwrap();
    let names = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert!(names.contains(&"recovery_status".to_owned()), "{names:?}");

    let denied = fixture_server(vec![Capability::SessionRead]).await;
    let listed = denied
        .handle_message(request(96, "tools/list", json!({})))
        .await
        .unwrap();
    let names = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert!(!names.contains(&"recovery_status".to_owned()), "{names:?}");
}

#[tokio::test]
async fn static_resources_are_listed_and_readable() {
    let server = fixture_server(vec![Capability::ArtifactRead]).await;
    let listed = server
        .handle_message(request(2, "resources/list", json!({})))
        .await
        .unwrap();
    let uris: Vec<String> = listed["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|resource| resource["uri"].as_str().unwrap().to_owned())
        .collect();
    for expected in [
        "bobby://capabilities",
        "bobby://failure-taxonomy",
        "bobby://intents",
        "bobby://primitives",
        "bobby://job-handlers",
    ] {
        assert!(uris.contains(&expected.to_owned()), "{expected} not listed");
        let read = server
            .handle_message(request(3, "resources/read", json!({"uri":expected})))
            .await
            .unwrap();
        assert!(
            read["result"]["contents"][0]["text"]
                .as_str()
                .is_some_and(|text| !text.is_empty()),
            "{expected} read returned no text"
        );
    }
}

/// The four `PrimitiveCommand` variants with no named tool are reachable only
/// through `command_execute`, whose advertised `envelope.command` is opaque, so
/// `bobby://primitives` is the only in-band description of their shape.
#[tokio::test]
async fn the_primitives_resource_documents_real_toolless_primitive_kinds() {
    const TOOLLESS_KINDS: &[&str] = &[
        "clickAndWaitForPopup",
        "clickAndWaitForDownload",
        "setFocusEmulation",
        "setEmulatedMedia",
    ];
    let definitions = mcp_gateway::definitions_for_test();
    let union_kinds: Vec<String> = definitions["PrimitiveCommand"]["oneOf"]
        .as_array()
        .expect("PrimitiveCommand is a oneOf union")
        .iter()
        .filter_map(|variant| variant["properties"]["kind"]["const"].as_str())
        .map(str::to_owned)
        .collect();
    assert!(
        union_kinds.len() > TOOLLESS_KINDS.len(),
        "PrimitiveCommand union looks wrong: {union_kinds:?}"
    );

    let server = fixture_server(vec![Capability::ArtifactRead]).await;
    let read = server
        .handle_message(request(
            2,
            "resources/read",
            json!({"uri":"bobby://primitives"}),
        ))
        .await
        .unwrap();
    let body = read["result"]["contents"][0]["text"]
        .as_str()
        .expect("bobby://primitives has a body")
        .to_owned();

    for kind in TOOLLESS_KINDS {
        assert!(
            union_kinds.iter().any(|existing| existing == kind),
            "bobby://primitives documents `{kind}`, which is no longer a \
             PrimitiveCommand variant: {union_kinds:?}"
        );
        assert!(
            body.contains(kind),
            "bobby://primitives does not describe `{kind}`"
        );
    }
    assert!(
        body.contains("command_execute"),
        "bobby://primitives does not say how these primitives are reached"
    );
}

/// A notification method is not a declared capability, so `initialize` cannot
/// advertise the push channel; the only in-band pointers are `events_read`'s
/// description and the failure taxonomy. Method names come from `notify`'s
/// constants, so a rename fails here rather than stranding the documentation.
#[tokio::test]
async fn the_pushed_event_channel_is_discoverable_in_band() {
    let server = fixture_server(vec![Capability::SessionRead, Capability::ArtifactRead]).await;
    let listed = server
        .handle_message(request(2, "tools/list", json!({})))
        .await
        .unwrap();
    let events_read = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "events_read")
        .expect("events_read is advertised")
        .clone();
    let description = events_read["description"].as_str().unwrap();
    assert!(
        description.contains(mcp_gateway::notify::EVENT_METHOD),
        "events_read still teaches only the poll: {description}"
    );
    assert!(
        description.contains("block"),
        "events_read does not say it blocks, but is marked readOnlyHint: {description}"
    );
    assert_eq!(
        events_read["annotations"]["readOnlyHint"],
        json!(true),
        "events_read stopped being read-only; revisit the blocking warning"
    );

    let read = server
        .handle_message(request(
            3,
            "resources/read",
            json!({"uri":"bobby://failure-taxonomy"}),
        ))
        .await
        .unwrap();
    let taxonomy = read["result"]["contents"][0]["text"].as_str().unwrap();
    for expected in [
        mcp_gateway::notify::EVENT_METHOD,
        mcp_gateway::notify::TOOLS_LIST_CHANGED_METHOD,
        mcp_gateway::notify::GAP_KIND,
    ] {
        assert!(
            taxonomy.contains(expected),
            "bobby://failure-taxonomy does not mention {expected}"
        );
    }
}

#[tokio::test]
async fn an_unknown_bobby_uri_is_rejected() {
    let server = fixture_server(vec![Capability::ArtifactRead]).await;
    let read = server
        .handle_message(request(2, "resources/read", json!({"uri":"bobby://nope"})))
        .await
        .unwrap();
    assert!(
        read["error"].is_object(),
        "unknown bobby:// uri was accepted"
    );
}

#[tokio::test]
async fn prompts_are_advertised_in_initialize() {
    let server = fixture_server(vec![Capability::SessionRead]).await;
    let response = server
        .handle_message(request(
            9,
            "initialize",
            json!({
                "protocolVersion":"2025-11-25",
                "capabilities":{},
                "clientInfo":{"name":"test","version":"1"}
            }),
        ))
        .await
        .unwrap();
    assert!(
        response["result"]["capabilities"]["prompts"].is_object(),
        "prompts capability is not advertised"
    );
}

#[tokio::test]
async fn the_form_prompt_binds_its_arguments() {
    let server = fixture_server(vec![Capability::SessionRead]).await;
    let listed = server
        .handle_message(request(2, "prompts/list", json!({})))
        .await
        .unwrap();
    let names: Vec<String> = listed["result"]["prompts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|prompt| prompt["name"].as_str().unwrap().to_owned())
        .collect();
    assert!(names.contains(&"fill_and_submit_form".to_owned()));

    let got = server
        .handle_message(request(
            3,
            "prompts/get",
            json!({
                "name":"fill_and_submit_form",
                "arguments":{"sessionId":"s-1","pageId":"p-1"}
            }),
        ))
        .await
        .unwrap();
    let text = serde_json::to_string(&got["result"]["messages"]).unwrap();
    assert!(
        text.contains("s-1") && text.contains("p-1"),
        "arguments not bound"
    );
    assert!(
        text.contains("a11y_snapshot"),
        "loop does not start at a11y_snapshot"
    );
    assert!(
        text.contains("intent_submit_and_verify"),
        "loop never submits"
    );
    assert!(text.contains("checkpoint_save"), "loop never checkpoints");
    assert!(
        text.contains("needsReconciliation") && text.contains("NOT retry"),
        "boundary submit doesn't warn against retrying on needsReconciliation"
    );
    assert!(
        text.contains("evidenceRefs"),
        "checkpoint step doesn't name evidenceRefs"
    );
}

#[tokio::test]
async fn the_extract_prompt_binds_its_arguments_and_skips_the_checkpoint() {
    let server = fixture_server(vec![Capability::SessionRead]).await;
    let listed = server
        .handle_message(request(2, "prompts/list", json!({})))
        .await
        .unwrap();
    let names: Vec<String> = listed["result"]["prompts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|prompt| prompt["name"].as_str().unwrap().to_owned())
        .collect();
    assert!(names.contains(&"extract_from_page".to_owned()));

    let got = server
        .handle_message(request(
            3,
            "prompts/get",
            json!({
                "name":"extract_from_page",
                "arguments":{"sessionId":"s-2","pageId":"p-2"}
            }),
        ))
        .await
        .unwrap();
    let text = serde_json::to_string(&got["result"]["messages"]).unwrap();
    assert!(
        text.contains("s-2") && text.contains("p-2"),
        "arguments not bound"
    );
    assert!(
        text.contains("a11y_snapshot"),
        "loop does not start at a11y_snapshot"
    );
    assert!(text.contains("intent_extract"), "loop never extracts");
    // Extraction never mutates the page (Replayable), so the loop must not
    // introduce a checkpoint.
    assert!(
        !text.contains("checkpoint_save"),
        "read-only loop should not checkpoint"
    );
    assert!(
        !text.contains("intent_submit_and_verify"),
        "read-only loop should not submit"
    );
}

#[tokio::test]
async fn the_recover_prompt_binds_its_workflow_id() {
    let server = fixture_server(vec![Capability::SessionRead]).await;
    let listed = server
        .handle_message(request(2, "prompts/list", json!({})))
        .await
        .unwrap();
    let names: Vec<String> = listed["result"]["prompts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|prompt| prompt["name"].as_str().unwrap().to_owned())
        .collect();
    assert!(names.contains(&"recover_workflow".to_owned()));

    let got = server
        .handle_message(request(
            3,
            "prompts/get",
            json!({
                "name":"recover_workflow",
                "arguments":{"sessionId":"s-3","pageId":"p-3","workflowId":"w-3"}
            }),
        ))
        .await
        .unwrap();
    let text = serde_json::to_string(&got["result"]["messages"]).unwrap();
    assert!(
        text.contains("s-3") && text.contains("p-3") && text.contains("w-3"),
        "arguments not bound"
    );
    assert!(text.contains("recovery_status"), "loop never reads status");
    assert!(text.contains("workflow_recover"), "loop never recovers");
    assert!(
        text.contains("needsReconciliation"),
        "loop doesn't name the reconciliation decision"
    );
}

#[tokio::test]
async fn a_prompt_missing_a_required_argument_is_rejected() {
    let server = fixture_server(vec![Capability::SessionRead]).await;
    let got = server
        .handle_message(request(
            2,
            "prompts/get",
            json!({"name":"fill_and_submit_form","arguments":{"sessionId":"s-1"}}),
        ))
        .await
        .unwrap();
    assert!(got["error"].is_object(), "missing pageId was accepted");
}

#[tokio::test]
async fn an_unknown_prompt_name_is_rejected() {
    let server = fixture_server(vec![Capability::SessionRead]).await;
    let got = server
        .handle_message(request(
            2,
            "prompts/get",
            json!({
                "name":"does_not_exist",
                "arguments":{"sessionId":"s-1","pageId":"p-1"}
            }),
        ))
        .await
        .unwrap();
    assert!(got["error"].is_object(), "unknown prompt name was accepted");
}

// ---------------------------------------------------------------------------
// Server-to-client notifications
// ---------------------------------------------------------------------------

const NOTIFY_PRINCIPAL_A: uuid::Uuid = uuid!("10000000-0000-0000-0000-000000000090");
const NOTIFY_PRINCIPAL_B: uuid::Uuid = uuid!("10000000-0000-0000-0000-000000000091");

/// A `Server` bound to an explicit principal and an `EventStore` the test also
/// holds, matching the broker's shape: one retained log shared by every
/// principal's `Server`. Appending through that store is the same call
/// `Server::submit_envelope` and `POST /v1/commands` make.
async fn notify_fixture(principal: uuid::Uuid, events: EventStore) -> Server {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(principal),
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
    let server = Server::for_interface(runtime, handle, events, ArtifactResources::default());
    initialize(&server).await;
    server
}

async fn next_notification(stream: &mut mcp_gateway::NotificationStream) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(2), stream.recv())
        .await
        .expect("a notification arrives")
        .expect("the subscription is open")
}

#[tokio::test]
async fn runtime_events_reach_the_client_as_notifications() {
    let events = EventStore::new(64);
    let server = notify_fixture(NOTIFY_PRINCIPAL_A, events.clone()).await;
    let mut notifications = server.notifications().subscribe().await;

    events
        .append_for(
            PrincipalId::from_uuid(NOTIFY_PRINCIPAL_A),
            Event::new("command.outcome", json!({"commandId": "c-1"})),
        )
        .await;

    let frame = next_notification(&mut notifications).await;
    assert_eq!(frame["method"], "notifications/bobby/event");
    assert_eq!(frame["jsonrpc"], "2.0");
    assert!(frame["params"].is_object(), "{frame}");
    assert!(
        frame.get("id").is_none(),
        "a notification must carry no id: {frame}"
    );
    // `params` is the event body `GET /v1/events` returns, verbatim.
    assert_eq!(frame["params"]["kind"], "command.outcome");
    assert_eq!(frame["params"]["cursor"], 1);
    assert_eq!(frame["params"]["payload"]["commandId"], "c-1");
}

/// SECURITY: the notification stream must be principal-scoped or it is a
/// cross-principal data leak. `EventStore` partitions by audience and the only
/// read path a subscription can reach is `read_after_for`.
#[tokio::test]
async fn notifications_never_deliver_another_principals_events() {
    let events = EventStore::new(64);
    let server_a = notify_fixture(NOTIFY_PRINCIPAL_A, events.clone()).await;
    let server_b = notify_fixture(NOTIFY_PRINCIPAL_B, events.clone()).await;
    let mut stream_a = server_a.notifications().subscribe().await;
    let mut stream_b = server_b.notifications().subscribe().await;

    events
        .append_for(
            PrincipalId::from_uuid(NOTIFY_PRINCIPAL_B),
            Event::new("command.outcome", json!({"audience": "b"})),
        )
        .await;

    let frame = next_notification(&mut stream_b).await;
    assert_eq!(frame["params"]["payload"]["audience"], "b");

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), stream_a.recv())
            .await
            .is_err(),
        "principal A must never observe principal B's events"
    );

    // ...and A's stream is live, not merely broken: its own event still arrives.
    events
        .append_for(
            PrincipalId::from_uuid(NOTIFY_PRINCIPAL_A),
            Event::new("command.outcome", json!({"audience": "a"})),
        )
        .await;
    let frame = next_notification(&mut stream_a).await;
    assert_eq!(frame["params"]["payload"]["audience"], "a");
    assert_eq!(
        frame["params"]["cursor"], 2,
        "A resumes at its own event, never having been offered B's"
    );
}

/// Falling behind retention must be visible, then survivable. The frame carries
/// the same `EventGap` body and `event.gap` marker as `GET /v1/events?stream=1`,
/// but this stream re-arms instead of closing: `Server::serve` subscribes once
/// for the life of a stdio process, which cannot reconnect.
#[tokio::test]
async fn a_subscriber_that_falls_behind_retention_is_told_it_lost_events() {
    let events = EventStore::new(2);
    let server = notify_fixture(NOTIFY_PRINCIPAL_A, events.clone()).await;
    let mut notifications = server.notifications().subscribe().await;

    let principal = PrincipalId::from_uuid(NOTIFY_PRINCIPAL_A);
    for index in 0..5 {
        events
            .append_for(
                principal.clone(),
                Event::new("command.outcome", json!({"index": index})),
            )
            .await;
    }

    let frame = next_notification(&mut notifications).await;
    assert_eq!(frame["method"], "notifications/bobby/event");
    assert!(frame.get("id").is_none(), "{frame}");
    assert_eq!(frame["params"]["kind"], "event.gap");
    assert_eq!(frame["params"]["payload"]["reason"], "historyLost");
    assert_eq!(
        frame["params"]["payload"]["earliestAvailable"], 4,
        "the client is told exactly where the surviving history starts: {frame}"
    );

    // Having been told what it lost, the client keeps receiving what follows.
    events
        .append_for(
            principal,
            Event::new("command.outcome", json!({"index": 99})),
        )
        .await;
    let frame = next_notification(&mut notifications).await;
    assert_eq!(
        frame["params"]["kind"], "command.outcome",
        "the event stream must re-arm after a gap, not stay silent for the rest \
         of the session: {frame}"
    );
    assert_eq!(frame["params"]["payload"]["index"], 99, "{frame}");
}

#[tokio::test]
async fn initialize_advertises_that_the_tool_list_can_change() {
    let server = fixture_server(vec![Capability::SessionRead]).await;
    let response = server
        .handle_message(request(
            1,
            "initialize",
            json!({
                "protocolVersion":"2025-11-25",
                "capabilities":{},
                "clientInfo":{"name":"test","version":"1"}
            }),
        ))
        .await
        .expect("initialize returns a response");
    assert_eq!(
        response["result"]["capabilities"]["tools"]["listChanged"], true,
        "{response}"
    );
}

#[tokio::test]
async fn initialize_carries_agent_instructions() {
    let server = fixture_server(vec![Capability::SessionRead]).await;
    let response = server
        .handle_message(request(
            1,
            "initialize",
            json!({
                "protocolVersion":"2025-11-25",
                "capabilities":{},
                "clientInfo":{"name":"test","version":"1"}
            }),
        ))
        .await
        .expect("initialize returns a response");
    let instructions = response["result"]["instructions"]
        .as_str()
        .expect("instructions");
    assert_eq!(instructions, mcp_gateway::INITIALIZE_INSTRUCTIONS);
    assert!(
        instructions.len() <= 500,
        "instructions too long: {}",
        instructions.len()
    );
    assert!(instructions.contains("error.repair"), "{instructions}");
    assert!(instructions.contains("autoCheckpoint"), "{instructions}");
    for tool in [
        "workflow_start",
        "workflow_observe",
        "intent_complete_form",
        "intent_submit_and_verify",
    ] {
        assert!(
            instructions.contains(tool),
            "initialize instructions must advertise the complete standard form loop together: missing {tool}: {instructions}"
        );
    }
    assert!(
        instructions.contains("load") && instructions.contains("together"),
        "deferred-schema hosts need one-round loading guidance: {instructions}"
    );
}

#[tokio::test]
async fn start_browsing_prompt_requires_no_existing_ids() {
    let server = fixture_server(vec![Capability::SessionRead]).await;
    let listed = server
        .handle_message(request(2, "prompts/list", json!({})))
        .await
        .unwrap();
    let prompt = listed["result"]["prompts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|prompt| prompt["name"] == "start_browsing")
        .expect("start_browsing prompt");
    assert!(prompt["arguments"]
        .as_array()
        .unwrap()
        .iter()
        .all(|arg| arg["required"] != true));

    let got = server
        .handle_message(request(
            3,
            "prompts/get",
            json!({"name":"start_browsing","arguments":{"url":"https://example.com"}}),
        ))
        .await
        .unwrap();
    let text = serde_json::to_string(&got["result"]["messages"]).unwrap();
    assert!(text.contains("workflow_start"), "{text}");
    assert!(text.contains("workflow_observe"), "{text}");
    assert!(text.contains("https://example.com"), "{text}");
}

#[tokio::test]
async fn a_capability_change_tells_subscribed_clients_to_relist_tools() {
    let events = EventStore::new(64);
    let server = notify_fixture(NOTIFY_PRINCIPAL_A, events).await;
    let mut notifications = server.notifications().subscribe().await;

    server.notify_tools_list_changed();

    let frame = next_notification(&mut notifications).await;
    assert_eq!(frame["method"], "notifications/tools/list_changed");
    assert_eq!(frame["jsonrpc"], "2.0");
    assert!(
        frame.get("id").is_none(),
        "a notification must carry no id: {frame}"
    );
}

/// The stdio transport pushes frames the client never asked for, on the same
/// stdout it writes responses to, one whole frame per line.
#[tokio::test]
async fn the_stdio_transport_writes_notifications_as_unsolicited_frames() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let events = EventStore::new(64);
    let server = notify_fixture(NOTIFY_PRINCIPAL_A, events.clone()).await;
    let (client, server_io) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let (client_read, mut client_write) = tokio::io::split(client);
    let mut reader = BufReader::new(client_read);

    let driver = async {
        // A request and its response first, so the notification below has to
        // share the writer with real response traffic.
        client_write
            .write_all(
                format!(
                    "{}\n",
                    request(
                        41,
                        "tools/call",
                        json!({"name":"runtime_info","arguments":{}})
                    )
                )
                .as_bytes(),
            )
            .await
            .expect("request writes");

        // Drain the response before appending: a subscription starts at the store's
        // tail, so an event appended before `serve` subscribes is not this
        // subscriber's. The response proves the read loop, and its subscription,
        // are up.
        let mut frames = Vec::new();
        while !frames.iter().any(|frame: &Value| frame["id"] == json!(41)) {
            let mut line = String::new();
            reader.read_line(&mut line).await.expect("stdout reads");
            frames.push(serde_json::from_str(line.trim()).expect("one JSON object per line"));
        }

        events
            .append_for(
                PrincipalId::from_uuid(NOTIFY_PRINCIPAL_A),
                Event::new("command.outcome", json!({"commandId": "c-2"})),
            )
            .await;

        while !frames.iter().any(|frame: &Value| frame.get("id").is_none()) {
            let mut line = String::new();
            reader.read_line(&mut line).await.expect("stdout reads");
            frames.push(serde_json::from_str(line.trim()).expect("one JSON object per line"));
        }
        frames
    };

    let frames = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::select! {
            result = server.serve(server_read, server_write) => {
                panic!("serve returned before the notification arrived: {result:?}")
            }
            frames = driver => frames,
        }
    })
    .await
    .expect("a notification reaches stdout");

    let response = frames
        .iter()
        .find(|frame| frame["id"] == json!(41))
        .expect("the response to id 41 is on stdout");
    assert!(response["result"].is_object(), "{response}");
    let notification = frames
        .iter()
        .find(|frame| frame.get("id").is_none())
        .expect("an unsolicited notification is on stdout");
    assert_eq!(notification["method"], "notifications/bobby/event");
    assert_eq!(notification["params"]["payload"]["commandId"], "c-2");
}

/// A subscription must start at the store's tail, not at cursor 0. The retained
/// log is shared by every principal and `EventStore::read_after_for` reports
/// `HistoryLost` against the store-wide front of the deque, so after
/// `max_event_retention` appends a cursor-0 subscription gaps on its first read
/// and never delivers a runtime event again. MCP has no resume cursor, so
/// reconnecting reproduces it.
#[tokio::test]
async fn a_subscription_opened_after_retention_wrapped_still_receives_new_events() {
    let events = EventStore::new(2);
    let server = notify_fixture(NOTIFY_PRINCIPAL_A, events.clone()).await;

    // Another principal's traffic wraps retention, exactly as a busy broker's does.
    for index in 0..5 {
        events
            .append_for(
                PrincipalId::from_uuid(NOTIFY_PRINCIPAL_B),
                Event::new("command.outcome", json!({"index": index})),
            )
            .await;
    }

    let mut notifications = server.notifications().subscribe().await;

    events
        .append_for(
            PrincipalId::from_uuid(NOTIFY_PRINCIPAL_A),
            Event::new("command.outcome", json!({"commandId": "c-1"})),
        )
        .await;

    let frame = next_notification(&mut notifications).await;
    assert_eq!(
        frame["params"]["kind"], "command.outcome",
        "a subscription opened against a wrapped store must deliver new events, \
         not gap against history it never asked for: {frame}"
    );
    assert_eq!(frame["params"]["payload"]["commandId"], "c-1", "{frame}");
}

#[tokio::test]
async fn caller_pinned_command_and_attempt_ids_are_threaded_and_echoed() {
    let server = Server::new(Arc::new(authenticated_with_intents().await));
    initialize(&server).await;

    let command_id = CommandId::new().0.to_string();
    let attempt_id = AttemptId::new().0.to_string();
    let outcome = server
        .handle_message(request(
            76,
            "tools/call",
            json!({
                "name":"intent_locate",
                "arguments":{
                    "sessionId":SessionId::new().0.to_string(),
                    "pageId":types::PageId::new().0.to_string(),
                    "commandId":command_id,
                    "attemptId":attempt_id,
                    "purpose":"the search box"
                }
            }),
        ))
        .await
        .unwrap();
    // A Boundary command's pre-action checkpoint must name the exact ids the
    // submit will carry, so the server threads caller-pinned ids through
    // unchanged and echoes the attempt id for the checkpoint's attemptId.
    assert!(outcome["error"].is_null(), "{outcome}");
    assert_eq!(
        outcome["result"]["structuredContent"]["commandId"],
        json!(command_id),
        "{outcome}"
    );
    assert_eq!(
        outcome["result"]["structuredContent"]["attemptId"],
        json!(attempt_id),
        "{outcome}"
    );
}

#[tokio::test]
async fn static_resources_are_readable_without_artifact_read() {
    let server = fixture_server(vec![Capability::SessionRead]).await;

    let listed = server
        .handle_message(request(2, "resources/list", json!({})))
        .await
        .unwrap();
    assert!(listed["error"].is_null(), "{listed}");
    let uris: Vec<String> = listed["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|resource| resource["uri"].as_str().unwrap().to_owned())
        .collect();
    for expected in [
        "bobby://capabilities",
        "bobby://failure-taxonomy",
        "bobby://intents",
        "bobby://primitives",
        "bobby://job-handlers",
    ] {
        assert!(uris.contains(&expected.to_owned()), "{expected} not listed");
        let read = server
            .handle_message(request(3, "resources/read", json!({"uri":expected})))
            .await
            .unwrap();
        assert!(
            read["result"]["contents"][0]["text"]
                .as_str()
                .is_some_and(|text| !text.is_empty()),
            "{expected} read returned no text: {read}"
        );
    }
    // A principal without artifact:read sees no live artifact entries, and
    // reading one is still denied -- only the repair docs are ungated.
    assert!(
        !uris.iter().any(|uri| uri.starts_with("artifact://")),
        "artifact entries leaked to a principal without artifact:read"
    );
    let denied = server
        .handle_message(request(
            4,
            "resources/read",
            json!({"uri":"artifact://deadbeef"}),
        ))
        .await
        .unwrap();
    assert_eq!(
        denied["error"]["data"]["interfaceError"]["code"], "missingCapability",
        "{denied}"
    );
}

#[tokio::test]
async fn download_url_requires_and_threads_a_page_id() {
    let server = fixture_server(vec![Capability::BrowserMutate, Capability::FileDownload]).await;

    // The advertised schema names pageId: the executor requires one, and the
    // gateway used to send None unconditionally, failing every call.
    let listed = server
        .handle_message(request(2, "tools/list", json!({})))
        .await
        .unwrap();
    let tool = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "download_url")
        .expect("download_url is advertised");
    assert!(
        tool["inputSchema"]["oneOf"]
            .as_array()
            .expect("download_url scope branches")
            .iter()
            .any(|branch| branch["required"] == json!(["sessionId", "pageId", "workflowId"])),
        "download_url must advertise pageId as required: {tool}"
    );
    assert!(
        tool["inputSchema"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .all(|branch| branch["properties"]["saveAs"] == json!({"oneOf":[{"type":"string","minLength":1,"maxLength":4096},{"type":"null"}]})),
        "download_url must advertise optional saveAs: {tool}"
    );

    let outcome = server
        .handle_message(request(
            3,
            "tools/call",
            json!({
                "name":"download_url",
                "arguments":{
                    "sessionId":SessionId::new().0.to_string(),
                    "pageId":types::PageId::new().0.to_string(),
                    "url":"https://example.test/file",
                    "maxBytes":1024,
                    "saveAs":"/allowed/downloads/file"
                }
            }),
        ))
        .await
        .unwrap();
    // Downstream failure (unknown session) is fine; the old defect failed
    // every call with "pageId is required" before anything ran.
    assert!(
        !outcome.to_string().contains("pageId is required"),
        "{outcome}"
    );
    assert!(
        !outcome.to_string().contains("malformedArguments"),
        "{outcome}"
    );
}

#[tokio::test]
async fn context_neighbors_is_gated_on_context_read() {
    let page_reader = fixture_server(vec![Capability::PageRead]).await;
    let listed = page_reader
        .handle_message(request(80, "tools/list", json!({})))
        .await
        .unwrap();
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"context_ask"),
        "context_ask must stay visible to page:read: {names:?}"
    );
    assert!(
        !names.contains(&"context_neighbors"),
        "context_neighbors must be hidden without context:read: {names:?}"
    );
    let denied = page_reader
        .handle_message(request(81, "tools/call", json!({
            "name":"context_neighbors",
            "arguments":{"sessionId":SessionId::new().0.to_string(),"pageId":types::PageId::new().0.to_string(),"description":"Email address"}
        })))
        .await
        .unwrap();
    assert_eq!(denied["error"]["code"], -32601, "{denied}");

    let context_reader = fixture_server(vec![Capability::PageRead, Capability::ContextRead]).await;
    let listed = context_reader
        .handle_message(request(82, "tools/list", json!({})))
        .await
        .unwrap();
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"context_neighbors"),
        "context_neighbors must be visible with context:read: {names:?}"
    );
}

/// Idempotent retry has to actually replay over MCP.
///
/// The digest used to cover the whole `CommandEnvelope`, including `deadline`
/// and the per-attempt `commandId`/`attemptId`. The gateway mints all three
/// fresh on every dispatch and gives the caller no way to pin a deadline, so a
/// retry could never match its own first try: every retry took the conflict
/// branch. An agent that timed out and retried the way the tool told it to got
/// `idempotencyConflict` on a command that may already have landed.
#[tokio::test]
async fn a_retry_under_one_idempotency_key_replays_instead_of_dispatching_again() {
    let runtime = Arc::new(authenticated_with_intents().await);
    let server = Server::new(runtime.clone());
    initialize(&server).await;

    let session_id = SessionId::new().0.to_string();
    let page_id = types::PageId::new().0.to_string();
    let locate = |id: u64, purpose: &str| {
        request(
            id,
            "tools/call",
            json!({
                "name":"intent_locate",
                "arguments":{
                    "sessionId":session_id,
                    "pageId":page_id,
                    "purpose":purpose,
                    "idempotencyKey":"retry-after-timeout"
                }
            }),
        )
    };

    let first = server
        .handle_message(locate(80, "the search box"))
        .await
        .unwrap();
    assert!(first["error"].is_null(), "{first}");
    let dispatches_after_first = runtime.submit_dispatch_count();

    let retry = server
        .handle_message(locate(81, "the search box"))
        .await
        .unwrap();
    assert!(retry["error"].is_null(), "retry was rejected: {retry}");
    assert_eq!(
        retry["result"]["structuredContent"]["commandId"],
        first["result"]["structuredContent"]["commandId"],
        "the retry ran a new command instead of replaying the first"
    );
    assert_eq!(
        retry["result"]["structuredContent"]["status"],
        first["result"]["structuredContent"]["status"],
        "the retry returned a different outcome"
    );
    // A replay is the original attempt's outcome, so this call's workflow and
    // attempt ids must not be stamped onto it.
    assert!(
        retry["result"]["structuredContent"]["attemptId"].is_null(),
        "a replay claimed an attempt id that never ran: {retry}"
    );
    assert_eq!(
        runtime.submit_dispatch_count(),
        dispatches_after_first,
        "the retry dispatched a second time instead of replaying"
    );

    // The safety property survives: the same key over a different command is
    // still a conflict, because identity covers the command itself.
    let conflict = server
        .handle_message(locate(82, "the login button"))
        .await
        .unwrap();
    let code = conflict["error"]["data"]["interfaceError"]["code"]
        .as_str()
        .unwrap_or_default();
    assert_eq!(
        code, "idempotencyConflict",
        "a different command under the same key must still conflict: {conflict}"
    );
}

/// `recovery_status` takes exactly one key, and says so when it does not.
///
/// An agent that was compacted has no `workflowId` left, so it asks by
/// `sessionId` instead. Both keys at once are two different questions in one
/// call and neither is not a question at all; the validator's schema subset
/// cannot express exactly-one-of, so the handler enforces it by name rather
/// than picking a silent precedence.
#[tokio::test]
async fn recovery_status_requires_exactly_one_of_workflow_id_or_session_id() {
    let server = fixture_server(vec![Capability::RecoveryRead]).await;

    for (case, arguments) in [
        ("neither", json!({})),
        (
            "both",
            json!({
                "workflowId":types::WorkflowId::new().0.to_string(),
                "sessionId":SessionId::new().0.to_string()
            }),
        ),
    ] {
        let response = server
            .handle_message(request(
                90,
                "tools/call",
                json!({"name":"recovery_status","arguments":arguments}),
            ))
            .await
            .unwrap();
        assert_eq!(response["error"]["code"], -32602, "{case}: {response}");
        assert_eq!(
            response["error"]["data"]["reason"], "exactlyOneOfWorkflowIdOrSessionId",
            "{case}: {response}"
        );
    }

    // Asking by session is accepted as a shape -- it reaches the runtime
    // instead of being refused at the argument boundary.
    let by_session = server
        .handle_message(request(
            91,
            "tools/call",
            json!({
                "name":"recovery_status",
                "arguments":{"sessionId":SessionId::new().0.to_string()}
            }),
        ))
        .await
        .unwrap();
    assert_ne!(
        by_session["error"]["code"], -32602,
        "asking by sessionId must not be an argument error: {by_session}"
    );
}

/// `autoCheckpoint` mints the checkpoint the boundary gate demands.
///
/// The manual sequence is three calls -- pin commandId/attemptId,
/// `checkpoint_save` naming those exact ids, then submit. The gateway cannot
/// collapse it on its own: a `WorkflowCheckpoint` needs `restartUrl` and
/// `currentUrl`, and nothing on the runtime interface exposes live page state.
/// So the runtime mints it, and the saved checkpoint must match the envelope
/// on every field `Executor::validate` compares, or the gate rejects it.
#[tokio::test]
async fn auto_checkpoint_saves_a_checkpoint_matching_the_boundary_command() {
    let root = tempfile::tempdir().unwrap();
    let journal = Arc::new(
        JsonlJournal::open(root.path().join("journal.jsonl"))
            .await
            .unwrap(),
    );
    let workers = Arc::new(WorkerPool::new(4, Arc::new(MinimalFactory)));
    let checkpoint_store = checkpoint_store::CheckpointStore::open(root.path().join("checkpoints"))
        .await
        .unwrap();
    let runtime_service = RuntimeService::with_recovery(
        SessionManager::new(workers.clone()),
        PageRuntime::new_with_checkpoints(journal, workers, checkpoint_store.clone()),
        page_runtime::RecoveryCoordinator::new(checkpoint_store.clone()),
    );
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-0000000000c1")),
            [
                Capability::SessionWrite,
                Capability::PageWrite,
                Capability::BrowserMutate,
                Capability::IntentExecute,
                Capability::RecoveryWrite,
            ],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let server = Server::new(Arc::new(AuthenticatedRuntime::new(runtime_service, handle)));
    initialize(&server).await;

    let session = server
        .handle_message(request(
            100,
            "tools/call",
            json!({"name":"session_create","arguments":{"profile":"fixture"}}),
        ))
        .await
        .unwrap();
    let session_id = session_id_from_structured_content(&session);
    let page = server
        .handle_message(request(
            101,
            "tools/call",
            json!({
                "name":"page_open",
                "arguments":{"sessionId":session_id.0.to_string()}
            }),
        ))
        .await
        .unwrap();
    assert_eq!(page["result"]["isError"], false, "{page}");
    let page_id = page["result"]["structuredContent"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let workflow_id = types::WorkflowId::new().0.to_string();
    let submitted = server
        .handle_message(request(
            102,
            "tools/call",
            json!({
                "name":"intent_submit_and_verify",
                "arguments":{
                    "sessionId":session_id.0.to_string(),
                    "pageId":page_id,
                    "workflowId":workflow_id,
                    "purpose":"submit the application",
                    "expectedState":{
                        "condition":{"kind":"document","ready":"interactive"},
                        "timeoutMs":1000
                    },
                    "autoCheckpoint":true
                }
            }),
        ))
        .await
        .unwrap();
    assert!(submitted["error"].is_null(), "{submitted}");
    let structured = &submitted["result"]["structuredContent"];
    assert!(
        structured["checkpointId"].is_string(),
        "the minted checkpoint must be nameable to workflow_recover: {submitted}"
    );

    // The five fields `Executor::validate` compares, plus the class. A
    // checkpoint that misses any one of them is refused by the gate, so this
    // is what makes the one-call form work rather than merely return.
    let saved = checkpoint_store
        .load(&types::WorkflowId(workflow_id.parse().unwrap()))
        .await
        .expect("autoCheckpoint must persist a checkpoint under the envelope's workflow");
    assert_eq!(saved.session_id, session_id);
    assert_eq!(saved.page_id.0.to_string(), page_id);
    assert_eq!(saved.recovery_class, types::CommandClass::Boundary);
    assert_eq!(
        saved
            .boundary_command_id
            .as_ref()
            .map(|id| id.0.to_string()),
        structured["commandId"].as_str().map(str::to_owned),
        "the checkpoint must name the command it guards"
    );
    assert_eq!(
        saved.attempt_id.0.to_string(),
        structured["attemptId"].as_str().unwrap(),
        "the checkpoint must name the attempt that ran"
    );
}

#[tokio::test]
async fn popup_auto_checkpoint_saves_a_checkpoint_matching_the_boundary_command() {
    let root = tempfile::tempdir().unwrap();
    let journal = Arc::new(
        JsonlJournal::open(root.path().join("journal.jsonl"))
            .await
            .unwrap(),
    );
    let workers = Arc::new(WorkerPool::new(4, Arc::new(MinimalFactory)));
    let checkpoint_store = checkpoint_store::CheckpointStore::open(root.path().join("checkpoints"))
        .await
        .unwrap();
    let runtime_service = RuntimeService::with_recovery(
        SessionManager::new(workers.clone()),
        PageRuntime::new_with_checkpoints(journal, workers, checkpoint_store.clone()),
        page_runtime::RecoveryCoordinator::new(checkpoint_store.clone()),
    );
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-0000000000c2")),
            [
                Capability::SessionWrite,
                Capability::PageWrite,
                Capability::BrowserMutate,
                Capability::RecoveryWrite,
            ],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let server = Server::new(Arc::new(AuthenticatedRuntime::new(runtime_service, handle)));
    initialize(&server).await;

    let session = server
        .handle_message(request(
            100,
            "tools/call",
            json!({"name":"session_create","arguments":{"profile":"fixture"}}),
        ))
        .await
        .unwrap();
    let session_id = session_id_from_structured_content(&session);
    let page = server
        .handle_message(request(
            101,
            "tools/call",
            json!({
                "name":"page_open",
                "arguments":{"sessionId":session_id.0.to_string()}
            }),
        ))
        .await
        .unwrap();
    assert_eq!(page["result"]["isError"], false, "{page}");
    let page_id = page["result"]["structuredContent"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let workflow_id = types::WorkflowId::new().0.to_string();
    let submitted = server
        .handle_message(request(
            102,
            "tools/call",
            json!({
                "name":"click_and_wait_for_popup",
                "arguments":{
                    "sessionId":session_id.0.to_string(),
                    "pageId":page_id,
                    "workflowId":workflow_id,
                    "target":{"role":"button","accessibleName":"Connect"},
                    "timeoutMs":1000
                }
            }),
        ))
        .await
        .unwrap();
    // MinimalFactory has no real popup; the gate under test is the checkpoint
    // mint, which must happen before browser dispatch either way.
    let structured = &submitted["result"]["structuredContent"];
    assert!(
        structured["checkpointId"].is_string(),
        "click_and_wait_for_popup must autoCheckpoint by default: {submitted}"
    );
    let saved = checkpoint_store
        .load(&types::WorkflowId(workflow_id.parse().unwrap()))
        .await
        .expect("autoCheckpoint must persist a checkpoint under the envelope's workflow");
    assert_eq!(saved.recovery_class, types::CommandClass::Boundary);
    assert_eq!(
        saved
            .boundary_command_id
            .as_ref()
            .map(|id| id.0.to_string()),
        structured["commandId"].as_str().map(str::to_owned),
        "the checkpoint must name the popup command it guards"
    );
}

/// `autoCheckpoint` is sugar over the boundary gate, never a way around it.
///
/// Without a recovery coordinator there is nowhere to save a checkpoint, so
/// the call must fail rather than run the Boundary command unprotected. The
/// same runtime accepts the identical command with `autoCheckpoint` absent,
/// which is what makes this a statement about the checkpoint and not about
/// the fixture.
#[tokio::test]
async fn auto_checkpoint_refuses_the_submit_when_the_checkpoint_cannot_be_saved() {
    let server = Server::new(Arc::new(authenticated_with_intents().await));
    initialize(&server).await;

    let arguments = |auto: bool| {
        json!({
            "name":"intent_submit_and_verify",
            "arguments":{
                "sessionId":SessionId::new().0.to_string(),
                "pageId":types::PageId::new().0.to_string(),
                "purpose":"submit the application",
                "expectedState":{
                    "condition":{"kind":"document","ready":"interactive"},
                    "timeoutMs":1000
                },
                "autoCheckpoint":auto
            }
        })
    };

    let refused = server
        .handle_message(request(110, "tools/call", arguments(true)))
        .await
        .unwrap();
    assert!(
        !refused["error"].is_null(),
        "a checkpoint that cannot be saved must fail the submit: {refused}"
    );

    // Same command, same runtime, no autoCheckpoint: reaches dispatch. The
    // failure above is the checkpoint, not the fixture.
    let without = server
        .handle_message(request(111, "tools/call", arguments(false)))
        .await
        .unwrap();
    assert!(without["error"].is_null(), "{without}");
}
