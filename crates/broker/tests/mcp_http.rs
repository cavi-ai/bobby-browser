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
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
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

/// A cached `/v1/mcp` `Server` must not survive its principal's capability rotation:
/// a stale, capability-frozen `Server` would answer a second `initialize` with a
/// "duplicate initialize" error (see `mcp_gateway::server`'s `Lifecycle` state
/// machine), since the first `initialize` already advanced it past
/// `AwaitingInitialize`. Rebuilding on a rotated handle resets the lifecycle, so the
/// rotated bearer's `initialize` must succeed exactly like a brand-new principal's.
///
/// Mirrors `runtime_binding_cache_rebuilds_when_capabilities_change` in
/// `crates/broker/src/lib.rs`, but end-to-end over `/v1/mcp`: two bearers issued for
/// the same principal with different capability sets, rather than two `CapabilityHandle`s
/// built directly against an `AuthorityStore`.
#[tokio::test]
async fn mcp_server_cache_rebuilds_for_rotated_handle() {
    let (app, _authority, admin_bearer) = app_with_admin(4).await;
    let bearer_one = issue_bearer(&app, &admin_bearer, PRINCIPAL_C, &["session:read"]).await;

    let (status, body) = post_mcp(&app, &bearer_one, initialize_request(1)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["protocolVersion"], "2025-11-25", "{body}");

    // Same principal, a freshly issued bearer with an additional capability — the
    // token-rotation scenario the cache must not paper over.
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
