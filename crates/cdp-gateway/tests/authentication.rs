use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use cdp_gateway::{CdpGateway, DiscoveryError, MethodRegistry};
use chrono::{Duration, Utc};
use interface_core::{Authority, AuthorityStore};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use tower::ServiceExt;
use types::{Capability, PrincipalId};

#[tokio::test]
async fn discovery_and_upgrade_fail_closed_without_valid_bearer() {
    let authority = Arc::new(AuthorityStore::in_memory());
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [Capability::SessionRead],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let handle = authority.verify(&token).await.unwrap();
    let runtime = Arc::new(AuthenticatedRuntime::new(RuntimeService::default(), handle));
    let gateway = Arc::new(CdpGateway::new(
        authority.clone(),
        runtime,
        MethodRegistry::compiled(),
        "ws://127.0.0.1:9222",
    ));
    assert_eq!(
        gateway.version(None).await.unwrap_err(),
        DiscoveryError::Unauthorized
    );
    assert_eq!(
        gateway.version(Some("wrong")).await.unwrap_err(),
        DiscoveryError::Unauthorized
    );
    let version = gateway.version(Some(&token)).await.unwrap();
    assert!(version
        .web_socket_debugger_url
        .contains("/devtools/browser/"));
    assert_eq!(gateway.list(Some(&token)).await.unwrap().len(), 0);
    let path = version
        .web_socket_debugger_url
        .split("127.0.0.1:9222")
        .nth(1)
        .unwrap();
    assert!(gateway.upgrade(path, Some(&token)).await.is_ok());

    let router = gateway.router();
    let unauthorized = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/json/version")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(unauthorized.headers()["www-authenticate"], "Bearer");
    let authorized = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/json/version")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(authorized.status(), StatusCode::OK);
    let version_json: serde_json::Value =
        serde_json::from_slice(&to_bytes(authorized.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(version_json["Protocol-Version"], "1.3");
    assert!(version_json.get("Browser").is_some());
    assert!(version_json.get("protocolVersion").is_none());

    let playwright_discovery = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/json/version/")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(playwright_discovery.status(), StatusCode::OK);

    let missing_download_capability = router
        .oneshot(
            Request::builder()
                .uri("/v1/streams/opaque-missing")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        missing_download_capability.status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn revoked_connection_is_rejected_again_at_method_dispatch() {
    let authority = Arc::new(AuthorityStore::in_memory());
    let principal = PrincipalId::from_uuid(uuid::Uuid::new_v4());
    let token = authority
        .issue(
            principal.clone(),
            [Capability::SessionRead],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let handle = authority.verify(&token).await.unwrap();
    let runtime = Arc::new(AuthenticatedRuntime::new(RuntimeService::default(), handle));
    let gateway = CdpGateway::new(
        authority.clone(),
        runtime,
        MethodRegistry::compiled(),
        "ws://localhost",
    );
    let version = gateway.version(Some(&token)).await.unwrap();
    let path = version
        .web_socket_debugger_url
        .strip_prefix("ws://localhost")
        .unwrap();
    let connection = gateway.upgrade(path, Some(&token)).await.unwrap();
    authority.revoke(&principal).await.unwrap();
    let response = connection
        .dispatch(cdp_gateway::CdpRequest::new(
            1,
            "Target.getTargets",
            serde_json::json!({}),
        ))
        .await;
    assert_eq!(
        response.error().unwrap().code,
        cdp_gateway::CdpErrorCode::RuntimeFailure as i32
    );
}
