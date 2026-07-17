use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use broker::{router, AppState};
use chrono::{Duration, SecondsFormat, Utc};
use config::InterfaceConfig;
use interface_core::{AuthorityStore, RuntimeInterface};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use tower::ServiceExt;
use types::{Capability, PrincipalId, CURRENT_INTERFACE_VERSION};
use uuid::uuid;

async fn authenticated_app(
    capabilities: impl IntoIterator<Item = Capability>,
    interface: InterfaceConfig,
) -> (axum::Router, String) {
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000010")),
            capabilities,
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let runtime = RuntimeService::default();
    let app = router(AppState::new(
        Arc::new(authority),
        move |handle| {
            Arc::new(AuthenticatedRuntime::new(runtime.clone(), handle))
                as Arc<dyn RuntimeInterface>
        },
        interface,
    ));
    (app, token)
}

fn authorized(method: &str, uri: &str, token: &str, body: Body) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("x-interface-version", CURRENT_INTERFACE_VERSION)
        .header("x-correlation-id", "10000000-0000-0000-0000-000000000011")
        .header(
            "x-deadline",
            (Utc::now() + Duration::minutes(2)).to_rfc3339_opts(SecondsFormat::Millis, true),
        )
        .header("content-type", "application/json")
        .body(body)
        .unwrap()
}

#[tokio::test]
async fn healthz_is_minimal_and_legacy_routes_do_not_exist() {
    let (app, _) = authenticated_app([], InterfaceConfig::default()).await;
    let health = app
        .clone()
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    let body = to_bytes(health.into_body(), 1024).await.unwrap();
    assert_eq!(body.as_ref(), br#"{"ok":true}"#);

    for path in ["/runtime", "/sessions", "/pages", "/navigate", "/commands"] {
        let response = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn body_limit_runs_before_deserialization_and_runtime_dispatch() {
    let interface = InterfaceConfig {
        max_request_bytes: 64,
        ..InterfaceConfig::default()
    };
    let (app, token) = authenticated_app([Capability::SessionWrite], interface).await;
    let body = serde_json::json!({
        "profile": "x".repeat(256),
        "proxy": null
    });

    let response = app
        .oneshot(authorized(
            "POST",
            "/v1/sessions",
            &token,
            Body::from(serde_json::to_vec(&body).unwrap()),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["code"], "invalidRequest");
    assert_eq!(
        json["error"]["correlationId"],
        "10000000-0000-0000-0000-000000000011"
    );
}

#[tokio::test]
async fn duplicate_protocol_headers_fail_closed() {
    let (app, token) =
        authenticated_app([Capability::SessionRead], InterfaceConfig::default()).await;

    for header in [
        "x-interface-version",
        "x-correlation-id",
        "x-deadline",
        "idempotency-key",
    ] {
        let mut request = authorized("GET", "/v1/runtime", &token, Body::empty());
        if header == "idempotency-key" {
            request
                .headers_mut()
                .append(header, "first".parse().unwrap());
        }
        request
            .headers_mut()
            .append(header, "duplicate".parse().unwrap());
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{header}"
        );
        if header == "idempotency-key" {
            let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                json["error"]["correlationId"],
                "10000000-0000-0000-0000-000000000011"
            );
        }
    }
}

#[tokio::test]
async fn invalid_json_is_a_structured_422_with_the_caller_correlation() {
    let (app, token) =
        authenticated_app([Capability::SessionWrite], InterfaceConfig::default()).await;
    let response = app
        .oneshot(authorized(
            "POST",
            "/v1/sessions",
            &token,
            Body::from("not-json"),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["code"], "invalidRequest");
    assert_eq!(
        json["error"]["correlationId"],
        "10000000-0000-0000-0000-000000000011"
    );
}

#[tokio::test]
async fn successful_responses_identify_the_interface_version_and_correlation() {
    let (app, token) =
        authenticated_app([Capability::SessionRead], InterfaceConfig::default()).await;
    let response = app
        .oneshot(authorized("GET", "/v1/runtime", &token, Body::empty()))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["x-interface-version"],
        CURRENT_INTERFACE_VERSION
    );
    assert_eq!(
        response.headers()["x-correlation-id"],
        "10000000-0000-0000-0000-000000000011"
    );
}

#[tokio::test]
async fn deadlines_are_strictly_bounded_before_dispatch() {
    let (app, token) =
        authenticated_app([Capability::SessionRead], InterfaceConfig::default()).await;
    let mut request = authorized("GET", "/v1/runtime", &token, Body::empty());
    request.headers_mut().insert(
        "x-deadline",
        (Utc::now() + Duration::days(1))
            .to_rfc3339_opts(SecondsFormat::Millis, true)
            .parse()
            .unwrap(),
    );

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["code"], "invalidRequest");
}
