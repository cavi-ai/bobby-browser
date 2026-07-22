use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use broker::testing::{app_with_admin, context_headers, issue_bearer};
use chrono::{Duration, SecondsFormat, Utc};
use tower::ServiceExt;
use uuid::uuid;

#[tokio::test]
async fn admin_issues_and_revokes_a_principal() {
    let (app, _authority, admin_bearer) = app_with_admin(8).await;
    let principal = uuid!("10000000-0000-0000-0000-000000000051");
    let expires_at =
        (Utc::now() + Duration::minutes(10)).to_rfc3339_opts(SecondsFormat::Millis, true);

    let issue_body = serde_json::json!({
        "principalId": principal,
        "capabilities": ["session:read", "session:write"],
        "expiresAt": expires_at,
    });
    let issue_request = context_headers(Request::post("/v1/principals"), &admin_bearer)
        .header("idempotency-key", "issue-principal-51")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&issue_body).unwrap()))
        .unwrap();
    let response = app.clone().oneshot(issue_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(json["bearer"].as_str().is_some(), "{json}");
    assert_eq!(json["principalId"], serde_json::json!(principal));
    assert_eq!(
        json["capabilities"],
        serde_json::json!(["session:read", "session:write"])
    );
    assert_eq!(json["expiresAt"], serde_json::json!(expires_at));
    let issued_bearer = json["bearer"].as_str().unwrap().to_owned();

    let list_request = context_headers(Request::get("/v1/sessions"), &issued_bearer)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(list_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let forbidden_body = serde_json::json!({
        "principalId": uuid!("10000000-0000-0000-0000-000000000052"),
        "capabilities": ["session:read"],
        "expiresAt": expires_at,
    });
    let forbidden_request = context_headers(Request::post("/v1/principals"), &issued_bearer)
        .header("idempotency-key", "issue-principal-52")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&forbidden_body).unwrap()))
        .unwrap();
    let response = app.clone().oneshot(forbidden_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let delete_request = context_headers(
        Request::delete(format!("/v1/principals/{principal}")),
        &admin_bearer,
    )
    .body(Body::empty())
    .unwrap();
    let response = app.clone().oneshot(delete_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let revoked_request = context_headers(Request::get("/v1/sessions"), &issued_bearer)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(revoked_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn issuance_is_bounded() {
    let (app, _authority, admin_bearer) = app_with_admin(8).await;
    let valid_expiry =
        (Utc::now() + Duration::minutes(10)).to_rfc3339_opts(SecondsFormat::Millis, true);

    let issue = |principal: uuid::Uuid,
                 capabilities: serde_json::Value,
                 expires_at: String,
                 idempotency_key: &'static str| {
        let app = app.clone();
        let admin_bearer = admin_bearer.clone();
        async move {
            let body = serde_json::json!({
                "principalId": principal,
                "capabilities": capabilities,
                "expiresAt": expires_at,
            });
            let request = context_headers(Request::post("/v1/principals"), &admin_bearer)
                .header("idempotency-key", idempotency_key)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap();
            app.oneshot(request).await.unwrap()
        }
    };

    // (a) minting authority:admin over HTTP is rejected.
    let response = issue(
        uuid!("10000000-0000-0000-0000-000000000061"),
        serde_json::json!(["authority:admin"]),
        valid_expiry.clone(),
        "issuance-bound-admin-cap",
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // (b) capabilities the issuer itself does not hold are rejected.
    let response = issue(
        uuid!("10000000-0000-0000-0000-000000000062"),
        serde_json::json!(["file:upload"]),
        valid_expiry.clone(),
        "issuance-bound-superset-cap",
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // (c) empty capability sets are rejected.
    let response = issue(
        uuid!("10000000-0000-0000-0000-000000000063"),
        serde_json::json!([]),
        valid_expiry.clone(),
        "issuance-bound-empty-caps",
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // (d) expiries beyond the max token TTL are rejected.
    let far_future_expiry =
        (Utc::now() + Duration::days(365 * 100)).to_rfc3339_opts(SecondsFormat::Millis, true);
    let response = issue(
        uuid!("10000000-0000-0000-0000-000000000064"),
        serde_json::json!(["session:read"]),
        far_future_expiry,
        "issuance-bound-long-expiry",
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // A compliant issuance still succeeds after all the above rejections.
    let response = issue(
        uuid!("10000000-0000-0000-0000-000000000065"),
        serde_json::json!(["session:read"]),
        valid_expiry,
        "issuance-bound-compliant",
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn revoking_one_principal_leaves_others_valid() {
    let (app, _authority, admin_bearer) = app_with_admin(8).await;
    let principal_a = uuid!("10000000-0000-0000-0000-000000000041");
    let principal_b = uuid!("10000000-0000-0000-0000-000000000042");
    let bearer_a = issue_bearer(&app, &admin_bearer, principal_a, &["session:read"]).await;
    let bearer_b = issue_bearer(&app, &admin_bearer, principal_b, &["session:read"]).await;

    let delete_request = context_headers(
        Request::delete(format!("/v1/principals/{principal_a}")),
        &admin_bearer,
    )
    .body(Body::empty())
    .unwrap();
    let response = app.clone().oneshot(delete_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let request_a = context_headers(Request::get("/v1/sessions"), &bearer_a)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(request_a).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    let request_b = context_headers(Request::get("/v1/sessions"), &bearer_b)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(request_b).await.unwrap().status(),
        StatusCode::OK
    );
}
