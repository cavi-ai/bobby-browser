//! `all_capabilities` and `list_tools` are shared helpers kept `pub` for reuse across the
//! tests here, so an unused one must not trip `dead_code`.
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::sync::Arc;

use chrono::{Duration, Utc};
use interface_core::AuthorityStore;
use mcp_gateway::Server;
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use types::{
    AttemptId, Capability, CheckpointId, CommandEnvelope, CommandId, CommandOutcome, Evidence,
    ExecutionPolicy, IntentCommand, IntentHints, LocateIntent, PageId, PageMode, PageState,
    PrincipalId, RecoveryDecision, RuntimeCommand, SessionId, SessionState, WorkflowId,
};
use uuid::uuid;

use mcp_gateway::TOOLS_LIST_BYTE_BUDGET;

pub fn all_capabilities() -> Vec<Capability> {
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

async fn fixture_server(capabilities: Vec<Capability>) -> Server {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000032")),
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
                "clientInfo":{"name":"budget","version":"1"}
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

pub async fn list_tools(capabilities: Vec<Capability>) -> Vec<Value> {
    let server = fixture_server(capabilities).await;
    let response = server
        .handle_message(request(2, "tools/list", json!({})))
        .await
        .expect("tools/list returns a response");
    response["result"]["tools"]
        .as_array()
        .expect("tools is an array")
        .clone()
}

/// Records the achieved size, not just that it is under the cap: `bobby doctor`'s handshake
/// check reports the same number against the same budget. `--nocapture` prints it; the
/// assertion message carries it on failure.
#[tokio::test]
async fn tools_list_stays_within_the_connect_budget() {
    let tools = list_tools(all_capabilities()).await;
    let bytes = serde_json::to_string(&tools).unwrap().len();
    let mut sizes = tools
        .iter()
        .map(|tool| {
            (
                serde_json::to_string(tool).unwrap().len(),
                tool["name"].as_str().unwrap_or("?").to_owned(),
            )
        })
        .collect::<Vec<_>>();
    sizes.sort_unstable();
    sizes.reverse();
    let breakdown = sizes
        .iter()
        .take(5)
        .map(|(size, name)| format!("{name}={size}"))
        .collect::<Vec<_>>()
        .join(" ");
    println!(
        "tools/list: {} tools, {bytes} bytes ({}% of the {TOOLS_LIST_BYTE_BUDGET} byte budget); \
         largest: {breakdown}",
        tools.len(),
        bytes * 100 / TOOLS_LIST_BYTE_BUDGET,
    );
    assert!(
        bytes <= TOOLS_LIST_BYTE_BUDGET,
        "tools/list is {bytes} bytes, over the {TOOLS_LIST_BYTE_BUDGET} byte budget; \
         largest: {breakdown}"
    );
}

#[tokio::test]
async fn tools_list_never_exceeds_the_frame_cap() {
    let tools = list_tools(all_capabilities()).await;
    let bytes = serde_json::to_string(&tools).unwrap().len();
    assert!(bytes < 1024 * 1024, "tools/list would exceed the frame cap");
}

#[tokio::test]
async fn every_advertised_tool_carries_a_name_and_input_schema() {
    for tool in list_tools(all_capabilities()).await {
        assert!(tool["name"].is_string(), "tool without a name: {tool}");
        assert!(
            tool["inputSchema"].is_object(),
            "tool without an inputSchema: {}",
            tool["name"]
        );
    }
}

#[tokio::test]
async fn command_execute_does_not_advertise_the_command_union() {
    let tools = list_tools(all_capabilities()).await;
    let tool = tools
        .iter()
        .find(|tool| tool["name"] == "command_execute")
        .expect("command_execute is advertised");
    let defs = &tool["inputSchema"]["$defs"];
    assert!(
        defs.get("PrimitiveCommand").is_none(),
        "command_execute still carries PrimitiveCommand"
    );
    assert!(
        defs.get("IntentCommand").is_none(),
        "command_execute still carries IntentCommand"
    );
    let bytes = serde_json::to_string(tool).unwrap().len();
    // The allowance covers the mandatory `outputSchema` every tool carries; the flattened
    // fallback command_execute uses stays under 1,000 bytes on its own.
    assert!(
        bytes < 3_500,
        "command_execute is {bytes} bytes, expected under 3500"
    );
}

#[tokio::test]
async fn command_execute_points_agents_at_the_named_tools() {
    let tools = list_tools(all_capabilities()).await;
    let tool = tools
        .iter()
        .find(|tool| tool["name"] == "command_execute")
        .expect("command_execute is advertised");
    let command = &tool["inputSchema"]["$defs"]["CommandEnvelope"]["properties"]["command"];
    assert_eq!(command["type"], "object");
    let description = command["description"]
        .as_str()
        .expect("the opaque command carries a description");
    assert!(
        description.contains("intent_") || description.contains("named tool"),
        "description does not point at the named tools: {description}"
    );
}

#[tokio::test]
async fn narrowing_the_advertised_schema_did_not_narrow_validation() {
    // `command_execute` advertises an opaque command but still rejects malformed nested
    // command content at the MCP edge (-32602) instead of dispatching it into the runtime.
    let server = fixture_server(all_capabilities()).await;
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

    let response = server
        .handle_message(request(
            5,
            "tools/call",
            json!({
                "name":"command_execute",
                "arguments":{"envelope":envelope_value}
            }),
        ))
        .await
        .expect("a response");
    assert_eq!(response["error"]["code"], -32602, "{response}");
}

#[tokio::test]
async fn checkpoint_save_does_not_advertise_the_evidence_union() {
    let tools = list_tools(all_capabilities()).await;
    let tool = tools
        .iter()
        .find(|tool| tool["name"] == "checkpoint_save")
        .expect("checkpoint_save is advertised");
    assert!(
        tool["inputSchema"]["$defs"].get("Evidence").is_none(),
        "checkpoint_save still carries the Evidence union"
    );
    assert!(
        tool["inputSchema"]["properties"]["evidenceRefs"].is_object(),
        "checkpoint_save does not accept evidenceRefs"
    );
    let bytes = serde_json::to_string(tool).unwrap().len();
    assert!(
        bytes < 13_000,
        "checkpoint_save is {bytes} bytes, expected under 13000"
    );
}

const READ_ONLY: &[&str] = &[
    "runtime_info",
    "session_list",
    "page_list",
    "inspect",
    "a11y_snapshot",
    "form_snapshot",
    "screenshot",
    "events_read",
    "recovery_status",
    "cookie_get",
    // Reads the retained page context; touches no page.
    "context_ask",
    // Changes only what `tools/list` advertises to this connection.
    "toolset_select",
    // Pure observers: they never mutate the page.
    "wait_for",
    "intent_locate",
    "intent_wait_for_state",
    "intent_extract",
];

const DESTRUCTIVE: &[&str] = &[
    "session_close",
    "page_close",
    "cookie_delete",
    "intent_submit_and_verify",
    "intent_follow",
];

// `command_execute` accepts an arbitrary `RuntimeCommand`, including `Navigate` and
// `DownloadUrl`, so it reaches the network exactly like the standalone tools below.
const OPEN_WORLD: &[&str] = &[
    "navigate",
    "download_url",
    "extract_structured",
    "command_execute",
    // Navigates when given a URL.
    "page_open",
    // Can carry the page to a new destination (verified by expected* guards).
    "click",
    "intent_follow",
    "intent_submit_and_verify",
];

// `idempotentHint` is unconditional under MCP: repeating the call with the same arguments
// must have no additional effect. An optional `idempotencyKey` does not establish that, so
// only tools that converge regardless of any key belong here.
const IDEMPOTENT: &[&str] = &["checkpoint_save", "emulate"];

/// `tool_title`'s wildcard arm (`annotations.rs`) returns this for any name with no explicit
/// arm. It must never fire for an advertised tool.
const UNTITLED_FALLBACK: &str = "Untitled tool";

#[tokio::test]
async fn every_tool_carries_a_title_and_annotations() {
    for tool in list_tools(all_capabilities()).await {
        let name = tool["name"].as_str().unwrap().to_owned();
        assert!(tool["title"].is_string(), "{name} has no title");
        assert_ne!(
            tool["title"],
            serde_json::json!(UNTITLED_FALLBACK),
            "{name} fell through to tool_title's fallback arm — add a real title in annotations.rs"
        );
        assert!(tool["annotations"].is_object(), "{name} has no annotations");
    }
}

#[tokio::test]
async fn idempotent_hints_match_the_spec_table() {
    for tool in list_tools(all_capabilities()).await {
        let name = tool["name"].as_str().unwrap();
        assert_eq!(
            tool["annotations"]["idempotentHint"] == serde_json::json!(true),
            IDEMPOTENT.contains(&name),
            "{name} idempotentHint disagrees with the spec table"
        );
    }
}

#[tokio::test]
async fn read_only_tools_are_marked_read_only() {
    let tools = list_tools(all_capabilities()).await;
    for tool in &tools {
        let name = tool["name"].as_str().unwrap();
        let read_only = tool["annotations"]["readOnlyHint"] == serde_json::json!(true);
        assert_eq!(
            read_only,
            READ_ONLY.contains(&name),
            "{name} readOnlyHint disagrees with the spec table"
        );
    }
}

#[tokio::test]
async fn destructive_and_open_world_hints_match_the_spec_table() {
    for tool in list_tools(all_capabilities()).await {
        let name = tool["name"].as_str().unwrap();
        assert_eq!(
            tool["annotations"]["destructiveHint"] == serde_json::json!(true),
            DESTRUCTIVE.contains(&name),
            "{name} destructiveHint disagrees with the spec table"
        );
        assert_eq!(
            tool["annotations"]["openWorldHint"] == serde_json::json!(true),
            OPEN_WORLD.contains(&name),
            "{name} openWorldHint disagrees with the spec table"
        );
    }
}

#[tokio::test]
async fn a_read_only_tool_is_never_also_destructive() {
    for tool in list_tools(all_capabilities()).await {
        let annotations = &tool["annotations"];
        let read_only = annotations["readOnlyHint"] == serde_json::json!(true);
        let destructive = annotations["destructiveHint"] == serde_json::json!(true);
        assert!(
            !(read_only && destructive),
            "{} is both read-only and destructive",
            tool["name"]
        );
    }
}

const DESCRIPTION_MAX_CHARS: usize = 400;

#[tokio::test]
async fn descriptions_stay_under_the_cap() {
    for tool in list_tools(all_capabilities()).await {
        let name = tool["name"].as_str().unwrap();
        let description = tool["description"].as_str().unwrap_or_default();
        assert!(!description.is_empty(), "{name} has an empty description");
        assert!(
            description.chars().count() <= DESCRIPTION_MAX_CHARS,
            "{name} description is {} chars, over the {DESCRIPTION_MAX_CHARS} cap",
            description.chars().count()
        );
    }
}

#[tokio::test]
async fn every_description_names_its_required_capability() {
    for tool in list_tools(all_capabilities()).await {
        let name = tool["name"].as_str().unwrap();
        let description = tool["description"].as_str().unwrap_or_default();
        assert!(
            description.contains("Requires "),
            "{name} description does not name its capability: {description}"
        );
    }
}

#[tokio::test]
async fn mutating_tools_describe_a_repair_action() {
    for tool in list_tools(all_capabilities()).await {
        let name = tool["name"].as_str().unwrap();
        if tool["annotations"]["readOnlyHint"] == serde_json::json!(true) {
            continue;
        }
        let description = tool["description"].as_str().unwrap_or_default();
        assert!(
            description.contains("On failure"),
            "{name} description has no repair clause: {description}"
        );
    }
}

// --- Output schemas ---------------------------------------------------------

/// Collects `$ref` targets. Matches only refs prefixed `#/$defs/`, the one form `schema.rs`
/// ever emits.
fn collect_refs(value: &Value, found: &mut BTreeSet<String>) {
    match value {
        Value::Object(fields) => {
            if let Some(name) = fields
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|reference| reference.strip_prefix("#/$defs/"))
            {
                found.insert(name.to_owned());
            }
            for field in fields.values() {
                collect_refs(field, found);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_refs(item, found);
            }
        }
        _ => {}
    }
}

/// Every `$ref` a schema carries must resolve inside that same schema's own `$defs`.
/// `reachable_definitions` drops a ref with a missing target instead of erroring, so a
/// dangling ref is otherwise invisible until the referencing definition becomes reachable
/// from a tool schema.
fn assert_no_dangling_refs(schema: &Value, label: &str) {
    let mut refs = BTreeSet::new();
    collect_refs(schema, &mut refs);
    let defs = schema.get("$defs").and_then(Value::as_object);
    for name in &refs {
        assert!(
            defs.is_some_and(|defs| defs.contains_key(name)),
            "{label} references #/$defs/{name}, which is absent from its own $defs"
        );
    }
}

#[tokio::test]
async fn no_schema_carries_a_ref_to_a_missing_definition() {
    for tool in list_tools(all_capabilities()).await {
        let name = tool["name"].as_str().unwrap().to_owned();
        assert_no_dangling_refs(&tool["inputSchema"], &format!("{name} inputSchema"));
        assert_no_dangling_refs(&tool["outputSchema"], &format!("{name} outputSchema"));
        let validation_schema = mcp_gateway::schema_for_test(&name);
        assert_no_dangling_refs(&validation_schema, &format!("{name} validation schema"));
    }
}

/// Table-level counterpart to `no_schema_carries_a_ref_to_a_missing_definition`, which only
/// walks emitted schemas. The fully-fielded `Evidence` entry in `definitions()` is reachable
/// from zero input schemas and zero advertised output schemas (every output narrows it to
/// `evidence_variant_tags()`), so its own refs go unchecked by that walk. This checks
/// `definitions()` against itself, independent of what any tool reaches.
#[tokio::test]
async fn every_definitions_table_entry_resolves_its_own_refs() {
    let defs = mcp_gateway::definitions_for_test();
    let defs = defs.as_object().expect("definitions() is an object");
    for (name, definition) in defs {
        let mut refs = BTreeSet::new();
        collect_refs(definition, &mut refs);
        for referenced in &refs {
            assert!(
                defs.contains_key(referenced),
                "definitions()[\"{name}\"] references #/$defs/{referenced}, \
                 which definitions() itself does not define"
            );
        }
    }
}

/// Every outputSchema is object-shaped. A non-object schema makes a conforming
/// client reject the whole `tools/list`, not just the offending tool, so this
/// assertion takes no exceptions.
#[tokio::test]
async fn tools_that_return_structured_content_declare_an_output_schema() {
    for tool in list_tools(all_capabilities()).await {
        let name = tool["name"].as_str().unwrap();
        assert!(
            tool["outputSchema"].is_object(),
            "{name} returns structuredContent but declares no outputSchema"
        );
        assert_eq!(
            tool["outputSchema"]["type"], "object",
            "{name} outputSchema is not an object schema"
        );
    }
}

#[tokio::test]
async fn output_schemas_carry_only_reachable_definitions() {
    for tool in list_tools(all_capabilities()).await {
        let name = tool["name"].as_str().unwrap();
        let Some(defs) = tool["outputSchema"]
            .get("$defs")
            .and_then(|d| d.as_object())
        else {
            continue;
        };
        let body = serde_json::to_string(&tool["outputSchema"]).unwrap();
        for key in defs.keys() {
            assert!(
                body.contains(&format!("#/$defs/{key}")),
                "{name} outputSchema carries unreachable definition {key}"
            );
        }
    }
}

/// Drift guard: `Evidence`'s hand-written union must advertise exactly the `kind` variants
/// `types::Evidence` serializes, no more and no fewer.
///
/// `Evidence` is reachable only from `workflow_recover`'s output schema, never from an input
/// or validation schema, and `schema_parity.rs`'s helpers only reach input schemas. So the
/// guard lives here, next to `list_tools`, which reads the genuine advertised `outputSchema`.
///
/// Limit: this compares `kind` tags only, because the advertised `Evidence` is itself a
/// tag-only projection (`evidence_variant_tags` in `schema.rs`). Field-level drift within a
/// variant is caught by the `*_round_trips_through_*` tests below.
#[tokio::test]
async fn evidence_variants_match_the_wire_type() {
    let generated_schema = serde_json::to_value(schemars::schema_for!(Evidence)).unwrap();
    let generated: BTreeSet<String> = generated_schema["oneOf"]
        .as_array()
        .expect("Evidence schema is a oneOf variant list")
        .iter()
        .map(|variant| {
            variant["properties"]["kind"]["const"]
                .as_str()
                .unwrap_or_else(|| panic!("Evidence variant must pin a kind const: {variant}"))
                .to_owned()
        })
        .collect();

    let tools = list_tools(all_capabilities()).await;
    let tool = tools
        .iter()
        .find(|tool| tool["name"] == "workflow_recover")
        .expect("workflow_recover is advertised");
    let hand_written: BTreeSet<String> = tool["outputSchema"]["$defs"]["Evidence"]["oneOf"]
        .as_array()
        .expect("Evidence oneOf must be an array")
        .iter()
        .map(|variant| {
            variant["properties"]["kind"]["const"]
                .as_str()
                .unwrap_or_else(|| panic!("Evidence variant must pin a kind const: {variant}"))
                .to_owned()
        })
        .collect();

    assert_eq!(generated, hand_written);
}

// --- Round trips: output schemas must accept real responses -----------------
//
// Everything above checks shape only: presence, `type`, `$defs` reachability, tag-set
// membership. Each test below builds a value the runtime would actually emit and validates
// it against that tool's own live, advertised `outputSchema`.
//
// These use the `jsonschema` crate rather than this crate's hand-rolled
// `validate`/`validate_at`: the hand-rolled engine ignores `allOf` entirely, so it cannot
// see a closed `$ref`'d schema sitting in `allOf` next to sibling properties.
fn find_tool<'a>(tools: &'a [Value], name: &str) -> &'a Value {
    tools
        .iter()
        .find(|tool| tool["name"] == name)
        .unwrap_or_else(|| panic!("{name} is not advertised"))
}

fn assert_validates(schema: &Value, value: &Value, label: &str) {
    let validator = jsonschema::validator_for(schema)
        .unwrap_or_else(|error| panic!("{label}'s outputSchema is not a valid schema: {error}"));
    if let Err(error) = validator.validate(value) {
        panic!(
            "{label}'s real response does not validate against its own \
             outputSchema: {error}\nvalue: {value}"
        );
    }
}

#[tokio::test]
async fn restarted_outcome_round_trips_through_the_fallback_output_schema() {
    let tools = list_tools(all_capabilities()).await;
    let click = find_tool(&tools, "click");
    let mut value = serde_json::to_value(CommandOutcome::Restarted {
        command_id: CommandId::new(),
        prior_attempt_id: AttemptId::new(),
        attempt_id: AttemptId::new(),
        reason: "boundary command re-run after a worker restart".to_owned(),
        evidence: vec![],
    })
    .unwrap();
    // `submit_envelope` (server.rs) always inserts this at the top level.
    value["workflowId"] = json!(WorkflowId::new());
    assert_validates(&click["outputSchema"], &value, "click (restarted outcome)");
}

#[tokio::test]
async fn completed_outcome_with_evidence_round_trips_through_the_fallback_output_schema() {
    let tools = list_tools(all_capabilities()).await;
    let navigate = find_tool(&tools, "navigate");
    let mut value = serde_json::to_value(CommandOutcome::Completed {
        command_id: CommandId::new(),
        evidence: vec![Evidence::Navigation {
            url: "https://example.test/".to_owned(),
            title: "Example".to_owned(),
        }],
    })
    .unwrap();
    value["workflowId"] = json!(WorkflowId::new());
    assert_validates(
        &navigate["outputSchema"],
        &value,
        "navigate (completed outcome with evidence)",
    );
}

#[tokio::test]
async fn artifact_registration_round_trips_through_the_fallback_output_schema() {
    let tools = list_tools(all_capabilities()).await;
    let screenshot = find_tool(&tools, "screenshot");
    let mut value = serde_json::to_value(CommandOutcome::Completed {
        command_id: CommandId::new(),
        evidence: vec![Evidence::Screenshot {
            artifact_id: "artifact-1".to_owned(),
            media_type: "image/png".to_owned(),
            width: 1024,
            height: 768,
            bytes: 4096,
            sha256: "a".repeat(64),
        }],
    })
    .unwrap();
    // Mirrors `ArtifactAdmission::apply_to_mcp_value` (resources.rs): a
    // partial admission inserts this top-level key alongside the outcome.
    value["artifactRegistration"] = json!({
        "status": "partial",
        "commandId": CommandId::new(),
        "attempted": 1,
        "admitted": 0,
        "failures": [{"kind": "screenshot", "code": "resourceExhausted"}],
        "retryable": false,
        "reconciliationRequired": true
    });
    value["workflowId"] = json!(WorkflowId::new());
    assert_validates(
        &screenshot["outputSchema"],
        &value,
        "screenshot (with artifactRegistration)",
    );
}

#[tokio::test]
async fn page_open_navigation_round_trips_through_its_output_schema() {
    let tools = list_tools(all_capabilities()).await;
    let page_open = find_tool(&tools, "page_open");
    let mut value = serde_json::to_value(PageState {
        id: PageId::new(),
        session_id: SessionId::new(),
        url: Some("https://example.test/".to_owned()),
        mode: PageMode::Interactive,
        ready_state: "complete".to_owned(),
        pending_requests: 0,
    })
    .unwrap();
    // Mirrors the `page_open` dispatch (server.rs): inserted whenever a URL
    // was given, regardless of whether navigation itself completed.
    value["navigationOutcome"] = serde_json::to_value(CommandOutcome::Completed {
        command_id: CommandId::new(),
        evidence: vec![],
    })
    .unwrap();
    assert_validates(&page_open["outputSchema"], &value, "page_open (navigated)");
}

#[tokio::test]
async fn session_list_round_trips_through_its_output_schema() {
    let tools = list_tools(all_capabilities()).await;
    let session_list = find_tool(&tools, "session_list");
    let sessions = serde_json::to_value(vec![SessionState {
        id: SessionId::new(),
        profile: "default".to_owned(),
        proxy: None,
        page_ids: vec![PageId::new()],
        created_at: Utc::now(),
        last_used_at: Utc::now(),
        execution_policy: ExecutionPolicy::default(),
    }])
    .unwrap();
    let value = serde_json::json!({"sessions": sessions});
    assert!(
        value["sessions"].is_array(),
        "session_list wraps its array under `sessions`"
    );
    assert_validates(&session_list["outputSchema"], &value, "session_list");
}

#[tokio::test]
async fn workflow_recover_resumed_evidence_round_trips_through_its_output_schema() {
    let tools = list_tools(all_capabilities()).await;
    let workflow_recover = find_tool(&tools, "workflow_recover");
    let value = serde_json::to_value(RecoveryDecision::Resumed {
        checkpoint_id: CheckpointId::new(),
        attempt_id: AttemptId::new(),
        evidence: vec![Evidence::Navigation {
            url: "https://example.test/".to_owned(),
            title: "Example".to_owned(),
        }],
    })
    .unwrap();
    assert_validates(
        &workflow_recover["outputSchema"],
        &value,
        "workflow_recover (resumed, with evidence)",
    );
}

/// The hand-rolled validator in `src/schema.rs` implements a subset of JSON Schema 2020-12
/// and ignores rather than rejects the keywords it does not implement. A schema that reaches
/// for `anyOf`, `allOf`, `not`, `if`/`then`/`else`, `patternProperties`, `dependentSchemas`,
/// `unevaluatedProperties`, `prefixItems`, `contains`, `uniqueItems`, `multipleOf`, or
/// `exclusiveMaximum` therefore builds green with a constraint that never runs on
/// `validate_tool_arguments`, the input-validation boundary for every `tools/call`.
///
/// The supported set is derived from the validator's own source rather than transcribed, so
/// it cannot rot when a `schema.get("…")` arm is added or dropped.
const SCHEMA_SOURCE: &str = include_str!("../src/schema.rs");

/// Bounds of the validator in `schema.rs`. Only this region is scanned for `.get("…")`:
/// schema construction also calls `.get`, and those keys are not validator support.
const VALIDATOR_REGION_START: &str = "\nfn validate_at(";
const VALIDATOR_REGION_END: &str = "\nfn accessibility_node(";

/// Keys that are annotations, not assertions: JSON Schema defines no validation behaviour
/// for them, so a validator that ignores them is correct.
const ANNOTATION_KEYWORDS: &[&str] = &["$schema", "description"];

/// Keys whose value is data rather than a subschema. Their contents are not
/// walked: an `enum` of objects would otherwise have its members' fields read
/// as if they were keywords.
const DATA_VALUED_KEYWORDS: &[&str] = &["const", "enum", "default", "examples"];

/// Keys whose value is a map of names to subschemas. The immediate children are
/// author-chosen names, not keywords (`definitions()` has a `TextMatch` variant with a
/// property literally named `contains`), so only the subschemas beneath them are walked.
const NAMED_SUBSCHEMA_MAPS: &[&str] = &["properties", "$defs"];

/// Every keyword the validator actually reads, scraped out of its own source.
fn validator_supported_keywords() -> BTreeSet<String> {
    let start = SCHEMA_SOURCE
        .find(VALIDATOR_REGION_START)
        .expect("validator region start not found — did validate_at get renamed?");
    let end = SCHEMA_SOURCE
        .find(VALIDATOR_REGION_END)
        .expect("validator region end not found — did accessibility_node get renamed?");
    assert!(
        start < end,
        "validator region is inverted; the anchors moved"
    );
    let region = &SCHEMA_SOURCE[start..end];
    let mut keywords = BTreeSet::new();
    let mut rest = region;
    while let Some(offset) = rest.find(".get(\"") {
        rest = &rest[offset + ".get(\"".len()..];
        let Some(close) = rest.find('"') else { break };
        keywords.insert(rest[..close].to_owned());
        rest = &rest[close..];
    }
    // Sanity-check the scrape: an empty or tiny set would make every assertion below pass
    // vacuously.
    for anchor in ["type", "properties", "required", "oneOf", "$ref", "items"] {
        assert!(
            keywords.contains(anchor),
            "keyword scrape missed `{anchor}` — the extraction, not the validator, is broken"
        );
    }
    assert!(
        keywords.len() >= 15,
        "keyword scrape found only {} keywords; the extraction is broken",
        keywords.len()
    );
    keywords
}

fn assert_only_supported_keywords(
    schema: &Value,
    label: &str,
    pointer: &str,
    supported: &BTreeSet<String>,
) {
    match schema {
        Value::Object(fields) => {
            for (key, value) in fields {
                if DATA_VALUED_KEYWORDS.contains(&key.as_str()) {
                    continue;
                }
                if NAMED_SUBSCHEMA_MAPS.contains(&key.as_str()) {
                    if let Some(children) = value.as_object() {
                        for (name, child) in children {
                            assert_only_supported_keywords(
                                child,
                                label,
                                &format!("{pointer}/{key}/{name}"),
                                supported,
                            );
                        }
                    }
                    continue;
                }
                assert!(
                    supported.contains(key) || ANNOTATION_KEYWORDS.contains(&key.as_str()),
                    "{label} declares `{key}` at {pointer} — the validator in \
                     crates/mcp-gateway/src/schema.rs does not implement it, so the \
                     constraint would be silently ignored on every tools/call. \
                     Implement it in validate_at/validate_object/validate_string/\
                     validate_number, or express the same constraint with a keyword \
                     the validator supports."
                );
                assert_only_supported_keywords(
                    value,
                    label,
                    &format!("{pointer}/{key}"),
                    supported,
                );
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                assert_only_supported_keywords(
                    item,
                    label,
                    &format!("{pointer}/{index}"),
                    supported,
                );
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn no_schema_uses_a_keyword_the_validator_does_not_implement() {
    let supported = validator_supported_keywords();
    // `schema_for_test` is what `validate_tool_arguments` enforces; `inputSchema` is what an
    // agent builds its arguments from. An ignored keyword is a silent hole in the first and
    // a false promise in the second.
    for tool in list_tools(all_capabilities()).await {
        let name = tool["name"].as_str().unwrap().to_owned();
        assert_only_supported_keywords(
            &tool["inputSchema"],
            &format!("{name} advertised inputSchema"),
            "",
            &supported,
        );
        assert_only_supported_keywords(
            &mcp_gateway::schema_for_test(&name),
            &format!("{name} validation schema"),
            "",
            &supported,
        );
    }
    // And the shared table both narrow from: an unreachable entry is one edit from being
    // reachable, and `$ref` resolution walks it with the same validator.
    let defs = mcp_gateway::definitions_for_test();
    for (name, definition) in defs.as_object().expect("definitions() is an object") {
        assert_only_supported_keywords(
            definition,
            &format!("definitions()[\"{name}\"]"),
            "",
            &supported,
        );
    }
}

/// `validate_string`'s `pattern` arm evaluates no regex; it hardcodes the character class of
/// the one pattern any schema declares, `sha256()`'s `^[0-9a-f]{64}$`. A second, different
/// `pattern` would be checked against that expression instead of its own.
#[tokio::test]
async fn the_only_declared_pattern_is_the_one_the_validator_implements() {
    const IMPLEMENTED_PATTERN: &str = "^[0-9a-f]{64}$";
    fn assert_patterns(schema: &Value, label: &str, pointer: &str) {
        match schema {
            Value::Object(fields) => {
                for (key, value) in fields {
                    if key == "pattern" {
                        assert_eq!(
                            value.as_str(),
                            Some(IMPLEMENTED_PATTERN),
                            "{label} declares pattern {value} at {pointer}, but \
                             validate_string in crates/mcp-gateway/src/schema.rs \
                             hardcodes {IMPLEMENTED_PATTERN} and would apply that \
                             instead. Implement real regex evaluation, or express \
                             the constraint another way."
                        );
                        continue;
                    }
                    if DATA_VALUED_KEYWORDS.contains(&key.as_str()) {
                        continue;
                    }
                    assert_patterns(value, label, &format!("{pointer}/{key}"));
                }
            }
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    assert_patterns(item, label, &format!("{pointer}/{index}"));
                }
            }
            _ => {}
        }
    }
    for tool in list_tools(all_capabilities()).await {
        let name = tool["name"].as_str().unwrap().to_owned();
        assert_patterns(
            &tool["inputSchema"],
            &format!("{name} advertised inputSchema"),
            "",
        );
        assert_patterns(&tool["outputSchema"], &format!("{name} outputSchema"), "");
        assert_patterns(
            &mcp_gateway::schema_for_test(&name),
            &format!("{name} validation schema"),
            "",
        );
    }
    let defs = mcp_gateway::definitions_for_test();
    for (name, definition) in defs.as_object().expect("definitions() is an object") {
        assert_patterns(definition, &format!("definitions()[\"{name}\"]"), "");
    }
}

/// The advertised `errorCode` enum must cover the runtime's full `ErrorCode`
/// vocabulary variant-for-variant; a missing variant means `tools/list`
/// describes a failure the agent cannot parse.
#[test]
fn advertised_error_codes_match_the_wire_vocabulary() {
    let generated = serde_json::to_value(schemars::schema_for!(types::ErrorCode)).unwrap();
    let wire: BTreeSet<String> = generated["enum"]
        .as_array()
        .expect("ErrorCode schema is an enum")
        .iter()
        .map(|variant| {
            variant
                .as_str()
                .expect("ErrorCode variant is a string")
                .to_owned()
        })
        .collect();
    let advertised: BTreeSet<String> = mcp_gateway::error_code_for_test()["enum"]
        .as_array()
        .expect("advertised error_code is an enum")
        .iter()
        .map(|variant| {
            variant
                .as_str()
                .expect("advertised ErrorCode variant is a string")
                .to_owned()
        })
        .collect();
    assert_eq!(wire, advertised);
}
