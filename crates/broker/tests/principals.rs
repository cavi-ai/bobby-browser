use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use broker::testing::{app_with_admin, context_headers, issue_bearer};
use chrono::{Duration, SecondsFormat, Utc};
use tower::ServiceExt;
use types::{
    AttemptId, CommandEnvelope, CommandId, InspectCommand, PageId, PrimitiveCommand,
    RuntimeCommand, SessionId, UploadFilesCommand, WorkflowId,
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
    let response_expiry =
        chrono::DateTime::parse_from_rfc3339(json["expiresAt"].as_str().expect("expiresAt string"))
            .expect("valid response expiry");
    let requested_expiry =
        chrono::DateTime::parse_from_rfc3339(&expires_at).expect("valid requested expiry");
    assert_eq!(response_expiry, requested_expiry);
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

// CaptureSink installs a thread-local tracing subscriber, so keep this async
// test on one runtime thread across the authority persistence awaits.
#[tokio::test(flavor = "current_thread")]
async fn principal_issuance_and_revocation_emit_audit_events() {
    let sink = observability::test_support::CaptureSink::install();
    let (app, _authority, admin_bearer) = app_with_admin(8).await;
    let principal = uuid!("10000000-0000-0000-0000-000000000061");
    let expires_at =
        (Utc::now() + Duration::minutes(10)).to_rfc3339_opts(SecondsFormat::Millis, true);

    let issue_body = serde_json::json!({
        "principalId": principal,
        "capabilities": ["session:read"],
        "expiresAt": expires_at,
    });
    let issue_request = context_headers(Request::post("/v1/principals"), &admin_bearer)
        .header("idempotency-key", "issue-principal-61")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&issue_body).unwrap()))
        .unwrap();
    let response = app.clone().oneshot(issue_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert!(
        sink.events()
            .iter()
            .any(|event| event["fields"]["message"] == "principal.issued"),
        "principal.issued audit event recorded"
    );

    let delete_request = context_headers(
        Request::delete(format!("/v1/principals/{principal}")),
        &admin_bearer,
    )
    .body(Body::empty())
    .unwrap();
    let response = app.clone().oneshot(delete_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(
        sink.events()
            .iter()
            .any(|event| event["fields"]["message"] == "principal.revoked"),
        "principal.revoked audit event recorded"
    );
}

// CaptureSink is thread-local; a current-thread runtime makes the leak check
// observe every event emitted while the request future is polled.
#[tokio::test(flavor = "current_thread")]
async fn bearer_token_never_appears_in_any_emitted_record() {
    let sink = observability::test_support::CaptureSink::install();
    let (app, _authority, admin_bearer) = app_with_admin(4).await;
    let request = context_headers(Request::get("/v1/runtime"), &admin_bearer)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    for event in sink.events() {
        assert!(
            !event.to_string().contains(&admin_bearer),
            "bearer token leaked into telemetry record: {event}"
        );
    }
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
    // Admin touches the runtime first.
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

/// A command envelope referencing a page that was never opened. Runtime validation rejects
/// it deterministically, so idempotency replay is exercised without a Chromium worker.
fn inspect_envelope(session_id: SessionId) -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id,
        page_id: Some(PageId::new()),
        deadline: Utc::now() + Duration::minutes(1),
        command: RuntimeCommand::Primitive(PrimitiveCommand::Inspect(InspectCommand::default())),
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

/// One `AuthenticatedRuntime`, and thus one `IdempotencyStore`, must serve a principal
/// across separate HTTP requests. Per `interface_core::idempotency` a non-retryable outcome
/// is retained: same key + same digest replays it, same key + different digest is a 409
/// `IdempotencyConflict`.
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

    // Non-retryable validation failure; the idempotency store retains it for replay.
    let (status1, body1) = submit_command(&app, &bearer_a, &envelope, key).await;
    assert_eq!(status1, StatusCode::UNPROCESSABLE_ENTITY, "{body1}");

    // Same key, same canonical digest: the stored outcome is returned verbatim.
    let (status2, body2) = submit_command(&app, &bearer_a, &envelope, key).await;
    assert_eq!(status2, status1);
    assert_eq!(body2, body1);

    // Same key, different digest, same principal: 409 only if the first reservation is
    // still live, i.e. the IdempotencyStore persisted across separate HTTP requests.
    let mismatched = inspect_envelope(session_a);
    let (status3, body3) = submit_command(&app, &bearer_a, &mismatched, key).await;
    assert_eq!(status3, StatusCode::CONFLICT, "{body3}");
    assert_eq!(body3["error"]["code"], "idempotencyConflict");

    // Keyspace is per principal: B reusing the same key string cannot collide with A.
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
        command: RuntimeCommand::Primitive(PrimitiveCommand::UploadFiles(UploadFilesCommand {
            selector: "input[type=file]".into(),
            target: None,
            paths: vec!["/tmp/example.txt".into()],
        })),
    }
}

/// `POST /v1/commands` must require `file:upload` for an `UploadFiles` primitive, not just
/// the coarse `browser:mutate` capability.
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
