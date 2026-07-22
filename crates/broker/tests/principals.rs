use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use broker::testing::{app_with_admin, context_headers, issue_bearer};
use chrono::{Duration, SecondsFormat, Utc};
use tower::ServiceExt;
use types::{
    AttemptId, CommandEnvelope, CommandId, InspectCommand, PageId, PrimitiveCommand, SessionId,
    UploadFilesCommand, WorkflowId,
};
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

#[tokio::test]
async fn principals_do_not_share_the_first_callers_binding() {
    let (app, _authority, admin_bearer) = broker::testing::app_with_admin(16).await;
    let team_bearer = broker::testing::issue_bearer(
        &app,
        &admin_bearer,
        uuid::Uuid::from_u128(9),
        &["session:read", "session:write"],
    )
    .await;
    // Admin touches the runtime first (this used to freeze the binding).
    let first = app
        .clone()
        .oneshot(
            context_headers(Request::get("/v1/sessions"), &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    // Team principal acts under ITS OWN handle.
    let created = app
        .clone()
        .oneshot(
            context_headers(Request::post("/v1/sessions"), &team_bearer)
                .header("content-type", "application/json")
                .header("idempotency-key", uuid::Uuid::new_v4().to_string())
                .body(Body::from(r#"{"profile":"default"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::OK);
    assert_eq!(
        broker::testing::last_bound_principal(),
        Some(uuid::Uuid::from_u128(9)),
        "runtime binding must use the caller's handle, not the first caller's"
    );
}

/// Creates a session for `bearer` via `POST /v1/sessions` and returns its id.
async fn create_session(app: &axum::Router, bearer: &str) -> SessionId {
    let request = context_headers(Request::post("/v1/sessions"), bearer)
        .header("content-type", "application/json")
        .header("idempotency-key", uuid::Uuid::new_v4().to_string())
        .body(Body::from(r#"{"profile":"default"}"#))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    serde_json::from_value(json["id"].clone()).expect("session response carries an id")
}

/// A command envelope that references a page which was never opened. The runtime's own
/// validation rejects it deterministically (`page does not exist`), which is enough to
/// exercise idempotency replay without needing a real Chromium worker.
fn inspect_envelope(session_id: SessionId) -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id,
        page_id: Some(PageId::new()),
        deadline: Utc::now() + Duration::minutes(1),
        command: PrimitiveCommand::Inspect(InspectCommand::default()),
    }
}

/// Submits `envelope` under `bearer` with idempotency-key `key` and returns the response
/// status and JSON body.
async fn submit_command(
    app: &axum::Router,
    bearer: &str,
    envelope: &CommandEnvelope,
    key: &str,
) -> (StatusCode, serde_json::Value) {
    let request = context_headers(Request::post("/v1/commands"), bearer)
        .header("content-type", "application/json")
        .header("idempotency-key", key)
        .body(Body::from(serde_json::to_vec(envelope).unwrap()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).expect("response body is valid JSON"),
    )
}

/// Regression test for the cached-runtime fix: idempotency replay/conflict detection
/// only works if the same `AuthenticatedRuntime` (and thus the same `IdempotencyStore`)
/// serves a principal's requests across the whole HTTP call, not just within one call.
///
/// Per `interface_core::idempotency`, a non-retryable outcome (like the validation
/// failure this test triggers) is retained by the store: a same-key/same-digest replay
/// returns the stored outcome, and a same-key/different-digest request conflicts
/// (`InterfaceErrorCode::IdempotencyConflict`, HTTP 409). The conflict case is the
/// decisive check here — it is observable *only* if the first request's reservation is
/// still present when the second request runs, which requires the store to have
/// survived between two separate HTTP round trips.
#[tokio::test]
async fn idempotent_command_replay_persists_and_is_scoped_per_principal() {
    let (app, _authority, admin_bearer) = app_with_admin(16).await;
    let bearer_a = issue_bearer(
        &app,
        &admin_bearer,
        uuid::Uuid::from_u128(31),
        &["session:write", "browser:mutate"],
    )
    .await;
    let bearer_b = issue_bearer(
        &app,
        &admin_bearer,
        uuid::Uuid::from_u128(32),
        &["session:write", "browser:mutate"],
    )
    .await;

    let session_a = create_session(&app, &bearer_a).await;
    let session_b = create_session(&app, &bearer_b).await;

    let key = "shared-idempotency-key-across-principals";
    let envelope = inspect_envelope(session_a.clone());

    // First submission fails validation (page was never opened) with a non-retryable
    // error, which the idempotency store retains for future replay.
    let (status1, body1) = submit_command(&app, &bearer_a, &envelope, key).await;
    assert_eq!(status1, StatusCode::UNPROCESSABLE_ENTITY, "{body1}");

    // Replay: identical key, identical envelope (same canonical digest) -> the stored
    // outcome is returned verbatim.
    let (status2, body2) = submit_command(&app, &bearer_a, &envelope, key).await;
    assert_eq!(status2, status1);
    assert_eq!(body2, body1);

    // Same key, a DIFFERENT envelope (fresh command id -> different canonical digest)
    // under the SAME principal: this can only produce a 409 if the first request's
    // reservation is still live, i.e. the IdempotencyStore genuinely persisted across
    // separate HTTP requests. Before the fix (a fresh AuthenticatedRuntime, and thus a
    // fresh IdempotencyStore, built on every bind call) this reservation would never
    // survive to be observed here — it would just execute independently.
    let mismatched = inspect_envelope(session_a);
    let (status3, body3) = submit_command(&app, &bearer_a, &mismatched, key).await;
    assert_eq!(status3, StatusCode::CONFLICT, "{body3}");
    assert_eq!(body3["error"]["code"], "idempotencyConflict");

    // Principal B reusing the exact same idempotency-key string on its own session is
    // unaffected: each principal is bound to its own cached runtime / IdempotencyStore
    // instance, so there is no shared keyspace to collide in.
    let envelope_b = inspect_envelope(session_b);
    let (status4, body4) = submit_command(&app, &bearer_b, &envelope_b, key).await;
    assert_eq!(status4, StatusCode::UNPROCESSABLE_ENTITY, "{body4}");
}

/// A `PrimitiveCommand::UploadFiles` envelope. Uploads move files across the host/browser
/// boundary and require `file:upload` in addition to `browser:mutate`.
fn upload_files_envelope(session_id: SessionId) -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id,
        page_id: Some(PageId::new()),
        deadline: Utc::now() + Duration::minutes(1),
        command: PrimitiveCommand::UploadFiles(UploadFilesCommand {
            selector: "input[type=file]".into(),
            target: None,
            paths: vec!["/tmp/example.txt".into()],
        }),
    }
}

/// Security regression: `submit_command` (`POST /v1/commands`) authorizes only the coarse
/// `browser:mutate` capability via `InterfaceOperation::SubmitCommand` before this fix, so
/// a principal holding `browser:mutate` (but not `file:upload`) could smuggle a privileged
/// `UploadFiles` primitive through the HTTP surface. This exercises the fix end-to-end
/// through the broker's real HTTP route, not just the sdk-core unit boundary.
#[tokio::test]
async fn upload_files_over_http_requires_file_upload_capability_not_just_browser_mutate() {
    let (app, _authority, admin_bearer) = app_with_admin(8).await;
    let bearer = issue_bearer(
        &app,
        &admin_bearer,
        uuid::Uuid::from_u128(41),
        &["session:write", "browser:mutate"],
    )
    .await;
    let session = create_session(&app, &bearer).await;

    let (status, body) = submit_command(
        &app,
        &bearer,
        &upload_files_envelope(session),
        "upload-without-file-upload-capability",
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "missingCapability", "{body}");
    assert_eq!(body["error"]["requiredCapability"], "file:upload", "{body}");
}
