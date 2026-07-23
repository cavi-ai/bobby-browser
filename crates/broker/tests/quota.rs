use std::time::Duration;

use axum::body::Body;
use axum::http::StatusCode;
use broker::testing::{app_with_admin_and_quota, context_headers, issue_bearer};
use tower::ServiceExt;
use uuid::uuid;

const PRINCIPAL_A: uuid::Uuid = uuid!("00000000-0000-0000-0000-000000000021");
const PRINCIPAL_B: uuid::Uuid = uuid!("00000000-0000-0000-0000-000000000022");

fn get(uri: &str, bearer: &str) -> axum::http::Request<Body> {
    context_headers(axum::http::Request::get(uri), bearer)
        .body(Body::empty())
        .expect("get request builds")
}

/// One principal's saturated in-flight quota must not block a different principal:
/// A's long-poll GET /v1/events parks (empty store, `after=0` never satisfies a gap
/// or a match — see `interface_core::events::read_decision` — so it waits on the
/// store's `Notify` until the request deadline), holding A's sole per-principal
/// permit (quota = 1) for the duration. A's second request must then be rejected
/// with 429 while B, a different principal, still succeeds.
#[tokio::test]
async fn one_principals_saturation_does_not_starve_another() {
    let (app, _authority, admin_bearer) = app_with_admin_and_quota(4, 1).await;
    let bearer_a = issue_bearer(&app, &admin_bearer, PRINCIPAL_A, &["session:read"]).await;
    let bearer_b = issue_bearer(&app, &admin_bearer, PRINCIPAL_B, &["session:read"]).await;

    let held = tokio::spawn(
        app.clone()
            .oneshot(get("/v1/events?after=0&limit=1", &bearer_a)),
    );
    tokio::time::sleep(Duration::from_millis(100)).await;

    let second_a = app
        .clone()
        .oneshot(get("/v1/sessions", &bearer_a))
        .await
        .expect("router accepts second A request");
    assert_eq!(second_a.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        second_a.headers().contains_key("retry-after"),
        "principal quota exhaustion must carry a retry-after header"
    );

    let b_response = app
        .oneshot(get("/v1/sessions", &bearer_b))
        .await
        .expect("router accepts B's request");
    assert_eq!(
        b_response.status(),
        StatusCode::OK,
        "principal B must not be starved by principal A's saturated quota"
    );

    held.abort();
}

/// A permit released after a request completes must be available to that same
/// principal's next request.
#[tokio::test]
async fn quota_releases_when_request_completes() {
    let (app, _authority, admin_bearer) = app_with_admin_and_quota(4, 1).await;
    let bearer_a = issue_bearer(&app, &admin_bearer, PRINCIPAL_A, &["session:read"]).await;

    let first = app
        .clone()
        .oneshot(get("/v1/sessions", &bearer_a))
        .await
        .expect("router accepts first A request");
    assert_eq!(first.status(), StatusCode::OK);

    let second = app
        .oneshot(get("/v1/sessions", &bearer_a))
        .await
        .expect("router accepts second A request");
    assert_eq!(
        second.status(),
        StatusCode::OK,
        "the permit held by the first, now-completed request must have been released"
    );
}
