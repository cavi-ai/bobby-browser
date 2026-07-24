use axum::body::Body;
use axum::http::{Request, StatusCode};
use broker::testing::{app_with_admin, context_headers};
use tower::ServiceExt;

#[tokio::test]
async fn authenticated_request_emits_span_scoped_completion_event() {
    let sink = observability::test_support::CaptureSink::install();
    let (app, _authority, admin_bearer) = app_with_admin(4).await;
    let request = context_headers(
        Request::builder().method("GET").uri("/v1/runtime"),
        &admin_bearer,
    )
    .body(Body::empty())
    .expect("request builds");
    let response = app.oneshot(request).await.expect("router responds");
    assert_eq!(response.status(), StatusCode::OK);
    let events = sink.events();
    let completed = events
        .iter()
        .find(|event| event["fields"]["message"] == "request.completed");
    let completed = completed.expect("request.completed event recorded");
    let spans = completed["spans"].as_array().expect("span context present");
    let request_span = spans
        .iter()
        .find(|span| span["name"] == "request")
        .expect("event emitted inside the request span");
    assert!(request_span["correlation_id"].is_string());
    assert!(request_span["principal_hash"].is_string());
    assert!(request_span["interface_version"].is_string());
    let correlation = request_span["correlation_id"].as_str().unwrap();
    assert!(
        uuid::Uuid::parse_str(correlation).is_ok(),
        "correlation id is a bare UUID string, got: {correlation}"
    );
}
