use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use broker::testing::{app_with_admin, context_headers, issue_bearer};
use serde_json::json;
use tower::ServiceExt;
use uuid::Uuid;

async fn wait_completed(app: &axum::Router, bearer: &str, job_id: &str) -> serde_json::Value {
    for _ in 0..50 {
        let req = context_headers(Request::get(format!("/v1/jobs/{job_id}")), bearer)
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        if json["status"] == "completed" || json["status"] == "failed" {
            return json;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("job {job_id} did not finish");
}

#[tokio::test]
async fn submit_echo_job_completes() {
    let (app, _, admin) = app_with_admin(8).await;
    let body = json!({
        "name": "echo",
        "payload": {"hello": "world"},
        "priority": "normal",
        "maxRetries": 1
    });
    let req = context_headers(Request::post("/v1/jobs"), &admin)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let created: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let job_id = created["jobId"].as_str().unwrap();

    let job = wait_completed(&app, &admin, job_id).await;
    assert_eq!(job["status"], "completed");
    assert_eq!(job["name"], "echo");
    assert_eq!(job["result"]["output"]["hello"], "world");
    assert!(job["correlationId"].as_str().is_some());
}

#[tokio::test]
async fn cancel_pending_job() {
    let (app, _, admin) = app_with_admin(8).await;
    let body = json!({ "name": "sleep", "payload": {"ms": 5000}, "maxRetries": 0 });
    let req = context_headers(Request::post("/v1/jobs"), &admin)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let created: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let job_id = created["jobId"].as_str().unwrap().to_string();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let cancel = context_headers(Request::delete(format!("/v1/jobs/{job_id}")), &admin)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(cancel).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let get = context_headers(Request::get(format!("/v1/jobs/{job_id}")), &admin)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(get).await.unwrap();
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let job: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(job["status"], "cancelled");
}

#[tokio::test]
async fn submit_is_idempotent_with_key() {
    let (app, _, admin) = app_with_admin(8).await;
    let body = json!({ "name": "echo", "payload": {"n": 1} });
    let key = "job-idem-1";
    let make = || {
        context_headers(Request::post("/v1/jobs"), &admin)
            .header("content-type", "application/json")
            .header("idempotency-key", key)
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    };
    let r1 = app.clone().oneshot(make()).await.unwrap();
    assert_eq!(r1.status(), StatusCode::CREATED);
    let b1 = to_bytes(r1.into_body(), 64 * 1024).await.unwrap();
    let j1: serde_json::Value = serde_json::from_slice(&b1).unwrap();

    let r2 = app.clone().oneshot(make()).await.unwrap();
    assert_eq!(r2.status(), StatusCode::CREATED);
    let b2 = to_bytes(r2.into_body(), 64 * 1024).await.unwrap();
    let j2: serde_json::Value = serde_json::from_slice(&b2).unwrap();
    assert_eq!(j1["jobId"], j2["jobId"]);
}

#[tokio::test]
async fn missing_job_capability_is_forbidden() {
    let (app, _, admin) = app_with_admin(8).await;
    let bearer = issue_bearer(
        &app,
        &admin,
        Uuid::from_u128(0x20000000000000000000000000000001),
        &["session:read"],
    )
    .await;
    let body = json!({ "name": "echo", "payload": {} });
    let req = context_headers(Request::post("/v1/jobs"), &bearer)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn jobs_are_invisible_and_immovable_across_principals() {
    let (app, _, admin) = app_with_admin(8).await;
    let body = json!({ "name": "echo", "payload": {"secret": "owner-only"}, "maxRetries": 0 });
    let req = context_headers(Request::post("/v1/jobs"), &admin)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let created: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let job_id = created["jobId"].as_str().unwrap();

    // A different principal holding job:read + job:cancel still cannot see
    // or cancel the admin's job: ownership answers as absence.
    let intruder = issue_bearer(&app, &admin, Uuid::new_v4(), &["job:read", "job:cancel"]).await;

    let get = context_headers(Request::get(format!("/v1/jobs/{job_id}")), &intruder)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(get).await.unwrap();
    // Ownership answers as absence; this API maps job not-found to 422.
    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "cross-principal read"
    );
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    assert!(
        !String::from_utf8_lossy(&bytes).contains("owner-only"),
        "payload must not leak cross-principal"
    );

    let cancel = context_headers(Request::delete(format!("/v1/jobs/{job_id}")), &intruder)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(cancel).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "cross-principal cancel"
    );

    // The owner is unaffected.
    let job = wait_completed(&app, &admin, job_id).await;
    assert_eq!(job["status"], "completed");
    assert_eq!(job["result"]["output"]["secret"], "owner-only");
}
