use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use broker::testing::{app_with_admin, app_with_admin_and_quota, context_headers, issue_bearer};
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::uuid;

const PRINCIPAL_A: uuid::Uuid = uuid!("00000000-0000-0000-0000-000000000031");
const PRINCIPAL_B: uuid::Uuid = uuid!("00000000-0000-0000-0000-000000000032");
const PRINCIPAL_C: uuid::Uuid = uuid!("00000000-0000-0000-0000-000000000033");

fn initialize_request(id: i64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "test-driver", "version": "1"}
        }
    })
}

fn initialized_notification() -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    })
}

fn tools_list_request(id: i64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/list",
        "params": {}
    })
}

fn tool_call_request(id: i64, name: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": name, "arguments": arguments}
    })
}

/// Sends one JSON-RPC message to `POST /v1/mcp` and returns the HTTP status plus the
/// parsed JSON body (empty object for a 202/empty body).
async fn post_mcp(app: &axum::Router, bearer: &str, body: Value) -> (StatusCode, Value) {
    let request = context_headers(Request::post("/v1/mcp"), bearer)
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&body).expect("body serializes"),
        ))
        .expect("mcp request builds");
    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("router accepts mcp request");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("mcp response body reads");
    let value = if bytes.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&bytes).expect("mcp response body is valid JSON")
    };
    (status, value)
}

fn get(uri: &str, bearer: &str) -> Request<Body> {
    context_headers(Request::get(uri), bearer)
        .body(Body::empty())
        .expect("get request builds")
}

#[tokio::test]
async fn mcp_over_http_initializes_and_lists_tools() {
    let (app, _authority, admin_bearer) = app_with_admin(4).await;
    let bearer = issue_bearer(
        &app,
        &admin_bearer,
        PRINCIPAL_A,
        &["session:read", "session:write"],
    )
    .await;

    let (status, body) = post_mcp(&app, &bearer, initialize_request(1)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["protocolVersion"], "2025-11-25", "{body}");

    let (status, _body) = post_mcp(&app, &bearer, initialized_notification()).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, body) = post_mcp(&app, &bearer, tools_list_request(2)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let tools = body["result"]["tools"]
        .as_array()
        .expect("tools/list result carries a tools array");
    let names = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    assert!(names.contains(&"runtime_info"), "{names:?}");
    assert!(names.contains(&"session_create"), "{names:?}");
}

#[tokio::test]
async fn mcp_requires_bearer_and_rejects_get() {
    let (app, _authority, _admin_bearer) = app_with_admin(4).await;
    let bogus_bearer = "bogus-bearer-that-is-definitely-not-issued-000000";

    let (status, _body) = post_mcp(&app, bogus_bearer, initialize_request(1)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let request = Request::get("/v1/mcp")
        .body(Body::empty())
        .expect("get request builds");
    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("router accepts get to /v1/mcp");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn mcp_get_opens_a_keepalive_sse_stream_for_authenticated_principals() {
    let (app, _authority, admin_bearer) = app_with_admin(4).await;
    let bearer = issue_bearer(&app, &admin_bearer, PRINCIPAL_A, &["session:read"]).await;
    let request = Request::get("/v1/mcp")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .expect("get request builds");
    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("router accepts get to /v1/mcp");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
}

#[tokio::test]
async fn two_principals_get_independent_mcp_lifecycles() {
    let (app, _authority, admin_bearer) = app_with_admin(4).await;
    let bearer_a = issue_bearer(&app, &admin_bearer, PRINCIPAL_A, &["session:read"]).await;
    let bearer_b = issue_bearer(&app, &admin_bearer, PRINCIPAL_B, &["session:read"]).await;

    let (status, body) = post_mcp(&app, &bearer_a, initialize_request(1)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["protocolVersion"], "2025-11-25", "{body}");

    // B never initialized: its own lifecycle must be gated, independent of A's.
    let (status, body) = post_mcp(&app, &bearer_b, tools_list_request(2)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["error"]["code"], -32002,
        "principal B must observe its own not-initialized lifecycle, not A's: {body}"
    );
}

#[tokio::test]
async fn mcp_respects_per_principal_quota() {
    let (app, _authority, admin_bearer) = app_with_admin_and_quota(16, 1).await;
    let bearer_a = issue_bearer(&app, &admin_bearer, PRINCIPAL_A, &["session:read"]).await;
    let bearer_b = issue_bearer(&app, &admin_bearer, PRINCIPAL_B, &["session:read"]).await;

    let held = tokio::spawn(
        app.clone()
            .oneshot(get("/v1/events?after=0&limit=1", &bearer_a)),
    );
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (status, body) = post_mcp(&app, &bearer_a, initialize_request(1)).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "A's saturated per-principal quota must reject the MCP request too: {body}"
    );

    let (status, body) = post_mcp(&app, &bearer_b, initialize_request(1)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "principal B must not be starved by A's saturated quota: {body}"
    );

    held.abort();
}

/// A cached `/v1/mcp` `Server` must not survive its principal's capability rotation.
/// A stale, capability-frozen `Server` would reject the rotated bearer's `initialize`
/// as a duplicate, since the first one advanced its `Lifecycle` past
/// `AwaitingInitialize`.
#[tokio::test]
async fn mcp_server_cache_rebuilds_for_rotated_handle() {
    let (app, _authority, admin_bearer) = app_with_admin(4).await;
    let bearer_one = issue_bearer(&app, &admin_bearer, PRINCIPAL_C, &["session:read"]).await;

    let (status, body) = post_mcp(&app, &bearer_one, initialize_request(1)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["protocolVersion"], "2025-11-25", "{body}");

    // Same principal, freshly issued bearer with an additional capability.
    let bearer_two = issue_bearer(
        &app,
        &admin_bearer,
        PRINCIPAL_C,
        &["session:read", "session:write"],
    )
    .await;

    let (status, body) = post_mcp(&app, &bearer_two, initialize_request(1)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "rotated bearer must get a fresh MCP lifecycle, not the stale cached Server: {body}"
    );
    assert_eq!(body["result"]["protocolVersion"], "2025-11-25", "{body}");
    assert!(
        body.get("error").is_none(),
        "expected a fresh initialize success, got a duplicate-initialize style error: {body}"
    );
}

/// `checkpoint_save` over `POST /v1/mcp` must run the same
/// `RuntimeInterface::resolve_command_evidence` the stdio transport does. A command id
/// with no journal record must come back as a `notFound` interface error, not a
/// schema-level `-32602` rejection before dispatch, and must fail closed rather than
/// silently contribute nothing.
#[tokio::test]
async fn checkpoint_save_resolves_evidence_refs_over_the_broker_http_transport() {
    let (app, _authority, admin_bearer) = app_with_admin(4).await;
    let bearer = issue_bearer(
        &app,
        &admin_bearer,
        PRINCIPAL_A,
        &[
            "session:read",
            "session:write",
            "page:write",
            "browser:mutate",
            "recovery:write",
        ],
    )
    .await;

    let (status, body) = post_mcp(&app, &bearer, initialize_request(1)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _body) = post_mcp(&app, &bearer, initialized_notification()).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, session) = post_mcp(
        &app,
        &bearer,
        tool_call_request(2, "session_create", json!({"profile": "http-checkpoint"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let session_id = session["result"]["structuredContent"]["id"]
        .as_str()
        .expect("session_create returns an id")
        .to_owned();

    let checkpoint = types::WorkflowCheckpoint {
        schema_version: types::WorkflowCheckpoint::SCHEMA_VERSION,
        checkpoint_id: types::CheckpointId::new(),
        workflow_id: types::WorkflowId::new(),
        attempt_id: types::AttemptId::new(),
        session_id: types::SessionId(uuid::Uuid::parse_str(&session_id).unwrap()),
        page_id: types::PageId::new(),
        restart_url: "https://example.test/".to_owned(),
        current_url: "https://example.test/".to_owned(),
        cursor: None,
        boundary_command_id: None,
        recovery_class: types::CommandClass::Replayable,
        invariants: vec![],
        replayable_inputs: vec![],
        evidence: vec![],
        recovery_history: vec![],
        recovery_receipts: vec![],
        created_at: chrono::Utc::now(),
    };
    let unowned_command_id = types::CommandId::new();
    let (status, response) = post_mcp(
        &app,
        &bearer,
        tool_call_request(
            3,
            "checkpoint_save",
            json!({"checkpoint": checkpoint, "evidenceRefs": [unowned_command_id]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_ne!(
        response["error"]["code"], -32602,
        "checkpoint_save over POST /v1/mcp must not fail with the old hardcoded \
         evidenceRefsUnresolvable rejection: {response}"
    );
    assert_eq!(
        response["error"]["data"]["interfaceError"]["code"], "notFound",
        "an evidenceRefs id with no journal record must resolve (and fail) against the \
         real runtime, not get rejected before dispatch: {response}"
    );
}

// ---------------------------------------------------------------------------
// `GET /v1/mcp`: server-to-client notifications
// ---------------------------------------------------------------------------

/// A router whose `EventStore` the test also holds, so it can append through the
/// same call the command route and the MCP tool surface make. Every per-principal
/// MCP `Server` shares that one store, as in production.
async fn app_with_events(
    events: interface_core::EventStore,
    principals: &[(uuid::Uuid, &[types::Capability])],
) -> (
    axum::Router,
    Vec<String>,
    std::sync::Arc<interface_core::AuthorityStore>,
) {
    let authority = std::sync::Arc::new(interface_core::AuthorityStore::in_memory());
    let mut bearers = Vec::new();
    for (principal, capabilities) in principals {
        bearers.push(issue_for(&authority, *principal, capabilities).await);
    }
    let runtime = sdk_core::RuntimeService::default();
    let app = broker::router(
        broker::AppState::new(
            authority.clone(),
            move |handle| {
                std::sync::Arc::new(sdk_core::AuthenticatedRuntime::new(runtime.clone(), handle))
                    as std::sync::Arc<dyn interface_core::RuntimeInterface>
            },
            config::InterfaceConfig::default(),
        )
        .with_boundaries(events, broker::ArtifactCatalog::default()),
    );
    (app, bearers, authority)
}

async fn issue_for(
    authority: &interface_core::AuthorityStore,
    principal: uuid::Uuid,
    capabilities: &[types::Capability],
) -> String {
    authority
        .issue(
            types::PrincipalId::from_uuid(principal),
            capabilities.to_vec(),
            chrono::Utc::now() + chrono::Duration::minutes(5),
        )
        .await
        .expect("bearer issues")
        .expose_once()
}

async fn open_mcp_stream(app: &axum::Router, bearer: &str) -> axum::body::BodyDataStream {
    let request = Request::get("/v1/mcp")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .expect("get request builds");
    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("router accepts get to /v1/mcp");
    assert_eq!(response.status(), StatusCode::OK);
    response.into_body().into_data_stream()
}

/// Reads SSE `data:` payloads off a live stream, skipping keep-alive comments.
/// Returns `None` if nothing arrives within `within`.
async fn next_mcp_frame(
    stream: &mut axum::body::BodyDataStream,
    within: Duration,
) -> Option<Value> {
    use futures_util::StreamExt;

    let deadline = tokio::time::Instant::now() + within;
    let mut buffer = String::new();
    loop {
        let chunk = tokio::time::timeout_at(deadline, stream.next())
            .await
            .ok()??;
        buffer
            .push_str(std::str::from_utf8(&chunk.expect("sse chunk reads")).expect("sse is utf-8"));
        while let Some(index) = buffer.find('\n') {
            let line = buffer[..index].trim_end().to_owned();
            buffer.drain(..=index);
            if let Some(data) = line.strip_prefix("data:") {
                return Some(
                    serde_json::from_str(data.trim_start()).expect("sse data line is JSON"),
                );
            }
        }
    }
}

#[tokio::test]
async fn the_mcp_get_stream_pushes_the_principals_runtime_events() {
    let events = interface_core::EventStore::new(64);
    let (app, bearers, _authority) = app_with_events(
        events.clone(),
        &[(PRINCIPAL_A, &[types::Capability::SessionRead])],
    )
    .await;
    let mut stream = open_mcp_stream(&app, &bearers[0]).await;

    events
        .append_for(
            types::PrincipalId::from_uuid(PRINCIPAL_A),
            interface_core::Event::new("command.outcome", json!({"commandId": "c-1"})),
        )
        .await;

    let frame = next_mcp_frame(&mut stream, Duration::from_secs(2))
        .await
        .expect("a notification frame arrives on the MCP stream");
    assert_eq!(frame["jsonrpc"], "2.0", "{frame}");
    assert_eq!(frame["method"], "notifications/bobby/event", "{frame}");
    assert!(
        frame.get("id").is_none(),
        "a notification must carry no id: {frame}"
    );
    assert_eq!(frame["params"]["kind"], "command.outcome", "{frame}");
    assert_eq!(frame["params"]["payload"]["commandId"], "c-1", "{frame}");
}

/// A subscription must start at the store's tail. The retained log is shared by
/// every principal and `HistoryLost` is judged against its store-wide front, so a
/// cursor-0 subscription gaps on its first read once `max_event_retention` appends
/// have been served, unrecoverably: MCP has no resume cursor.
#[tokio::test]
async fn an_mcp_stream_opened_after_retention_wrapped_still_receives_new_events() {
    let events = interface_core::EventStore::new(2);
    let (app, bearers, _authority) = app_with_events(
        events.clone(),
        &[
            (PRINCIPAL_A, &[types::Capability::SessionRead]),
            (PRINCIPAL_B, &[types::Capability::SessionRead]),
        ],
    )
    .await;

    // Another principal's traffic wraps retention, as a live broker's does.
    for index in 0..5 {
        events
            .append_for(
                types::PrincipalId::from_uuid(PRINCIPAL_B),
                interface_core::Event::new("command.outcome", json!({"index": index})),
            )
            .await;
    }

    let mut stream = open_mcp_stream(&app, &bearers[0]).await;
    events
        .append_for(
            types::PrincipalId::from_uuid(PRINCIPAL_A),
            interface_core::Event::new("command.outcome", json!({"commandId": "c-1"})),
        )
        .await;

    let frame = next_mcp_frame(&mut stream, Duration::from_secs(2))
        .await
        .expect("a frame arrives");
    assert_eq!(
        frame["params"]["kind"], "command.outcome",
        "a stream opened against a wrapped store must deliver new events, not a \
         terminal gap: {frame}"
    );
    assert_eq!(frame["params"]["payload"]["commandId"], "c-1", "{frame}");
}

/// CRITICAL: `GET /v1/mcp` scopes its stream to the authenticated principal.
/// A frame crossing principals here is a data leak, not a cosmetic defect.
#[tokio::test]
async fn the_mcp_get_stream_never_delivers_another_principals_events() {
    let events = interface_core::EventStore::new(64);
    let (app, bearers, _authority) = app_with_events(
        events.clone(),
        &[
            (PRINCIPAL_A, &[types::Capability::SessionRead]),
            (PRINCIPAL_B, &[types::Capability::SessionRead]),
        ],
    )
    .await;
    let mut stream_a = open_mcp_stream(&app, &bearers[0]).await;
    let mut stream_b = open_mcp_stream(&app, &bearers[1]).await;

    events
        .append_for(
            types::PrincipalId::from_uuid(PRINCIPAL_A),
            interface_core::Event::new("command.outcome", json!({"audience": "a"})),
        )
        .await;

    let frame = next_mcp_frame(&mut stream_a, Duration::from_secs(2))
        .await
        .expect("A receives its own event");
    assert_eq!(frame["params"]["payload"]["audience"], "a", "{frame}");
    assert!(
        next_mcp_frame(&mut stream_b, Duration::from_millis(400))
            .await
            .is_none(),
        "principal B's MCP stream must never carry principal A's events"
    );

    // `next_mcp_frame` cannot distinguish "nothing arrived" from "stream already
    // dead", so B's stream must be shown live at its own cursor.
    events
        .append_for(
            types::PrincipalId::from_uuid(PRINCIPAL_B),
            interface_core::Event::new("command.outcome", json!({"audience": "b"})),
        )
        .await;
    let frame = next_mcp_frame(&mut stream_b, Duration::from_secs(2))
        .await
        .expect("B's stream is live, not broken");
    assert_eq!(frame["params"]["payload"]["audience"], "b", "{frame}");
    assert_eq!(
        frame["params"]["cursor"], 2,
        "B resumes at its own event, never having been offered A's: {frame}"
    );
}

/// A principal that `GET /v1/events` and the `events_read` tool would both
/// refuse must not be handed the same events through the notification stream.
/// The channel still opens — MCP clients require it before they will POST.
#[tokio::test]
async fn the_mcp_get_stream_withholds_events_from_a_principal_without_subscribe_events() {
    let events = interface_core::EventStore::new(64);
    let (app, bearers, authority) = app_with_events(
        events.clone(),
        &[(PRINCIPAL_A, &[types::Capability::ArtifactRead])],
    )
    .await;
    let mut stream = open_mcp_stream(&app, &bearers[0]).await;

    events
        .append_for(
            types::PrincipalId::from_uuid(PRINCIPAL_A),
            interface_core::Event::new("command.outcome", json!({"commandId": "c-1"})),
        )
        .await;

    assert!(
        next_mcp_frame(&mut stream, Duration::from_millis(400))
            .await
            .is_none(),
        "events reached a principal that lacks session:read"
    );

    // Gated, not dead: control frames must still reach this client, or the
    // assertion above would pass against a broken channel too. Rotating the
    // principal's capabilities publishes one.
    let rotated = issue_for(
        &authority,
        PRINCIPAL_A,
        &[types::Capability::ArtifactRead, types::Capability::PageRead],
    )
    .await;
    let (status, body) = post_mcp(&app, &rotated, initialize_request(1)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let frame = next_mcp_frame(&mut stream, Duration::from_secs(2))
        .await
        .expect("the channel is open for control frames");
    assert_eq!(
        frame["method"], "notifications/tools/list_changed",
        "{frame}"
    );
}

/// Backs the `tools.listChanged: true` the `initialize` result advertises:
/// rotating a principal's capabilities rebuilds its cached `Server`, and the
/// client streaming off the old one is told to re-list before that stream ends.
#[tokio::test]
async fn rotating_capabilities_notifies_the_open_mcp_stream_to_relist_tools() {
    let (app, _authority, admin_bearer) = app_with_admin(4).await;
    let bearer_one = issue_bearer(&app, &admin_bearer, PRINCIPAL_C, &["session:read"]).await;

    // Establishes the cached `Server` this stream subscribes to.
    let (status, body) = post_mcp(&app, &bearer_one, initialize_request(1)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["result"]["capabilities"]["tools"]["listChanged"], true,
        "the server must advertise that its tool list can change: {body}"
    );
    let mut stream = open_mcp_stream(&app, &bearer_one).await;

    let bearer_two = issue_bearer(
        &app,
        &admin_bearer,
        PRINCIPAL_C,
        &["session:read", "session:write"],
    )
    .await;
    let (status, body) = post_mcp(&app, &bearer_two, initialize_request(2)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let frame = next_mcp_frame(&mut stream, Duration::from_secs(2))
        .await
        .expect("the rotated capability set reaches the open stream");
    assert_eq!(
        frame["method"], "notifications/tools/list_changed",
        "{frame}"
    );
    assert!(
        frame.get("id").is_none(),
        "a notification must carry no id: {frame}"
    );
    assert!(
        next_mcp_frame(&mut stream, Duration::from_millis(400))
            .await
            .is_none(),
        "the stream bound to the replaced Server must end, not keep serving a stale tool list"
    );
}
