//! MCP and HTTP answer the same context question identically for the same
//! principal, and both surfaces gate remembered-structure reads on
//! `context:read`.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::Request;
use chrono::{Duration, SecondsFormat, Utc};
use config::InterfaceConfig;
use interface_core::AuthorityStore;
use mcp_gateway::Server;
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use tower::ServiceExt;
use types::{Capability, PrincipalId, CURRENT_INTERFACE_VERSION};

async fn authority_with(capabilities: Vec<Capability>) -> (Arc<AuthorityStore>, String) {
    let authority = Arc::new(AuthorityStore::in_memory());
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            capabilities,
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    (authority, token)
}

fn http_request(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("x-interface-version", CURRENT_INTERFACE_VERSION)
        .header("x-correlation-id", "10000000-0000-0000-0000-000000000099")
        .header(
            "x-deadline",
            (Utc::now() + Duration::minutes(2)).to_rfc3339_opts(SecondsFormat::Millis, true),
        )
        .body(Body::empty())
        .unwrap()
}

async fn mcp_call(server: &Server, id: i64, name: &str, arguments: Value) -> Value {
    if id == 2 {
        server
            .handle_message(json!({
                "jsonrpc":"2.0","id":1,"method":"initialize",
                "params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"conformance","version":"1"}}
            }))
            .await
            .unwrap();
        server
            .handle_message(
                json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
            )
            .await;
    }
    server
        .handle_message(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}}))
        .await
        .unwrap()
}

#[tokio::test]
async fn context_answers_are_identical_over_mcp_and_http() {
    let (authority, token) = authority_with(vec![
        Capability::SessionWrite,
        Capability::PageRead,
        Capability::PageWrite,
        Capability::ContextRead,
        Capability::BrowserMutate,
    ])
    .await;
    let handle = authority.verify(&token).await.unwrap();
    // One shared inner runtime: MCP and HTTP must see the same session
    // ownership or the parity comparison is meaningless.
    let inner = RuntimeService::default();
    let server = Server::new(Arc::new(AuthenticatedRuntime::new(
        inner.clone(),
        handle.clone(),
    )));
    let app = broker::router(broker::AppState::new(
        authority.clone(),
        move |handle| Arc::new(AuthenticatedRuntime::new(inner.clone(), handle)),
        InterfaceConfig::default(),
    ));

    let session = mcp_call(&server, 2, "session_create", json!({"profile":"parity"})).await;
    let session_id = session["result"]["structuredContent"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("session_create failed: {session}"))
        .to_string();
    // A page id is enough: context_ask on an unknown page answers null on
    // every surface (no workers needed).
    let page_id = uuid::Uuid::new_v4().to_string();

    let mcp_answer = mcp_call(
        &server,
        4,
        "context_ask",
        json!({"sessionId":session_id,"pageId":page_id,"description":"Email address"}),
    )
    .await;
    let http_response = app
        .clone()
        .oneshot(http_request(
            &format!("/v1/context/ask?sessionId={session_id}&pageId={page_id}&description=Email%20address"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(http_response.status(), axum::http::StatusCode::OK);
    let body = to_bytes(http_response.into_body(), 16 * 1024)
        .await
        .unwrap();
    let http_answer: Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(
        mcp_answer["result"]["structuredContent"], http_answer,
        "MCP and HTTP context_ask answers diverged for the same principal"
    );

    // A principal without context:read is denied over HTTP.
    let thin_token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            vec![Capability::PageRead],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let thin_http = app
        .oneshot(http_request(
            &format!("/v1/context/ask?sessionId={session_id}&pageId={page_id}&description=Email%20address"),
            &thin_token,
        ))
        .await
        .unwrap();
    assert_eq!(thin_http.status(), axum::http::StatusCode::FORBIDDEN);
}
