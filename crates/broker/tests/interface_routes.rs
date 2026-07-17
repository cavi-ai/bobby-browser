use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use broker::{router, AppState};
use chrono::{Duration, SecondsFormat, Utc};
use config::InterfaceConfig;
use interface_core::{AuthorityStore, InterfaceResult, RuntimeInterface};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use tower::ServiceExt;
use types::{
    Capability, CommandEnvelope, CommandOutcome, CreateSessionRequest, Evidence, OpenPageRequest,
    PageState, PrincipalId, RecoveryDecision, RequestContext, RuntimeInfo, SessionState,
    WorkflowCheckpoint, WorkflowId, CURRENT_INTERFACE_VERSION,
};
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

#[derive(Clone)]
struct CountingRuntime {
    inner: AuthenticatedRuntime,
    calls: Arc<AtomicUsize>,
}

impl CountingRuntime {
    fn count(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl RuntimeInterface for CountingRuntime {
    async fn runtime_info(&self, ctx: RequestContext) -> InterfaceResult<RuntimeInfo> {
        self.count();
        self.inner.runtime_info(ctx).await
    }

    async fn list_sessions(&self, ctx: RequestContext) -> InterfaceResult<Vec<SessionState>> {
        self.count();
        self.inner.list_sessions(ctx).await
    }

    async fn create_session(
        &self,
        ctx: RequestContext,
        req: CreateSessionRequest,
    ) -> InterfaceResult<SessionState> {
        self.count();
        self.inner.create_session(ctx, req).await
    }

    async fn open_page(
        &self,
        ctx: RequestContext,
        req: OpenPageRequest,
    ) -> InterfaceResult<PageState> {
        self.count();
        self.inner.open_page(ctx, req).await
    }

    async fn submit(
        &self,
        ctx: RequestContext,
        envelope: CommandEnvelope,
    ) -> InterfaceResult<CommandOutcome> {
        self.count();
        self.inner.submit(ctx, envelope).await
    }

    async fn checkpoint(
        &self,
        ctx: RequestContext,
        checkpoint: WorkflowCheckpoint,
        evidence: Vec<Evidence>,
    ) -> InterfaceResult<WorkflowCheckpoint> {
        self.count();
        self.inner.checkpoint(ctx, checkpoint, evidence).await
    }

    async fn recover(
        &self,
        ctx: RequestContext,
        workflow: WorkflowId,
    ) -> InterfaceResult<RecoveryDecision> {
        self.count();
        self.inner.recover(ctx, workflow).await
    }
}

async fn counted_app(
    capabilities: impl IntoIterator<Item = Capability>,
    interface: InterfaceConfig,
) -> (axum::Router, String, Arc<AtomicUsize>) {
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
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let app = router(AppState::new(
        Arc::new(authority),
        move |handle| {
            Arc::new(CountingRuntime {
                inner: AuthenticatedRuntime::new(runtime.clone(), handle),
                calls: observed.clone(),
            }) as Arc<dyn RuntimeInterface>
        },
        interface,
    ));
    (app, token, calls)
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
    let (app, token, calls) = counted_app([Capability::SessionWrite], interface).await;
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
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn streamed_oversize_bodies_are_rejected_before_runtime_dispatch() {
    let interface = InterfaceConfig {
        max_request_bytes: 64,
        ..InterfaceConfig::default()
    };
    let (app, token, calls) = counted_app([Capability::SessionWrite], interface).await;
    let chunks = futures_util::stream::iter([
        Ok::<_, Infallible>("{\"profile\":\"".to_owned()),
        Ok::<_, Infallible>("x".repeat(128)),
        Ok::<_, Infallible>("\",\"proxy\":null}".to_owned()),
    ]);
    let response = app
        .oneshot(authorized(
            "POST",
            "/v1/sessions",
            &token,
            Body::from_stream(chunks),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn bodyless_routes_reject_declared_transferred_and_actual_bodies_before_dispatch() {
    let capabilities = [
        Capability::SessionRead,
        Capability::RecoveryRead,
        Capability::RecoveryWrite,
    ];
    let (app, token, calls) = counted_app(capabilities, InterfaceConfig::default()).await;
    let workflow = "10000000-0000-0000-0000-000000000099";

    let mut declared = authorized("GET", "/v1/sessions", &token, Body::empty());
    declared
        .headers_mut()
        .insert("content-length", "2".parse().unwrap());
    let mut transferred = authorized(
        "POST",
        &format!("/v1/recovery/{workflow}"),
        &token,
        Body::empty(),
    );
    transferred
        .headers_mut()
        .insert("transfer-encoding", "chunked".parse().unwrap());

    for request in [
        authorized("GET", "/v1/runtime", &token, Body::from("{}")),
        declared,
        transferred,
    ] {
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            response.headers()["x-interface-version"],
            CURRENT_INTERFACE_VERSION
        );
        assert_eq!(
            response.headers()["x-correlation-id"],
            "10000000-0000-0000-0000-000000000011"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn every_route_rejects_unexpected_duplicate_malformed_and_overlong_queries_before_dispatch() {
    let (app, token, calls) =
        counted_app([Capability::SessionRead], InterfaceConfig::default()).await;
    let overlong = format!("/v1/runtime?{}", "q".repeat(1025));

    for uri in [
        "/v1/runtime?unexpected=1".to_owned(),
        "/v1/sessions?a=1&a=2".to_owned(),
        "/v1/events?after=not-a-number".to_owned(),
        overlong,
    ] {
        let response = app
            .clone()
            .oneshot(authorized("GET", &uri, &token, Body::empty()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY, "{uri}");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
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
async fn authenticated_request_capacity_errors_keep_protocol_headers() {
    let interface = InterfaceConfig {
        max_connections: 1,
        ..InterfaceConfig::default()
    };
    let (app, token) = authenticated_app([Capability::SessionRead], interface).await;
    let waiting = tokio::spawn(app.clone().oneshot(authorized(
        "GET",
        "/v1/events?after=0&limit=1",
        &token,
        Body::empty(),
    )));
    tokio::task::yield_now().await;

    let response = app
        .oneshot(authorized("GET", "/v1/runtime", &token, Body::empty()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response.headers()["x-interface-version"],
        CURRENT_INTERFACE_VERSION
    );
    assert_eq!(
        response.headers()["x-correlation-id"],
        "10000000-0000-0000-0000-000000000011"
    );
    waiting.abort();
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
