use std::sync::{Arc, Mutex};

use axum::body::{to_bytes, Body};
use axum::http::{header::WWW_AUTHENTICATE, Request, StatusCode};
use broker::{router, AppState, EnrolledAuthority, StartupCredential};
use chrono::{Duration, SecondsFormat, Utc};
use config::InterfaceConfig;
use interface_core::{
    Authority, AuthorityStore, AuthorizationGuard, CapabilityHandle, RuntimeInterface,
};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use tower::ServiceExt;
use types::{
    AttemptId, Capability, CommandEnvelope, CommandId, InterfaceError, InterfaceOperation,
    PrimitiveCommand, PrincipalId, RuntimeCommand, WorkflowId, CURRENT_INTERFACE_VERSION,
};
use uuid::uuid;

fn protected_requests() -> Vec<Request<Body>> {
    [
        ("GET", "/v1/runtime"),
        ("GET", "/v1/sessions"),
        ("POST", "/v1/sessions"),
        ("POST", "/v1/pages"),
        ("POST", "/v1/commands"),
        ("POST", "/v1/checkpoints"),
        ("POST", "/v1/recovery/10000000-0000-0000-0000-000000000001"),
        ("GET", "/v1/events?after=0&limit=1"),
        ("GET", "/v1/artifacts/10000000-0000-0000-0000-000000000001"),
    ]
    .into_iter()
    .map(|(method, uri)| {
        Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap()
    })
    .collect()
}

fn app(
    authority: AuthorityStore,
    observed: Arc<Mutex<Option<AuthenticatedRuntime>>>,
) -> axum::Router {
    app_with_authority(Arc::new(authority), observed)
}

fn app_with_authority(
    authority: Arc<dyn Authority>,
    observed: Arc<Mutex<Option<AuthenticatedRuntime>>>,
) -> axum::Router {
    router(AppState::new(
        authority,
        move |handle| {
            let runtime = AuthenticatedRuntime::new(RuntimeService::default(), handle);
            *observed.lock().unwrap() = Some(runtime.clone());
            Arc::new(runtime) as Arc<dyn RuntimeInterface>
        },
        InterfaceConfig::default(),
    ))
}

#[derive(Clone)]
struct ExpiredBearerAuthority {
    store: AuthorityStore,
    expired_bearer: String,
}

#[async_trait::async_trait]
impl Authority for ExpiredBearerAuthority {
    async fn authenticate(
        &self,
        bearer: &str,
        now: chrono::DateTime<Utc>,
    ) -> Result<CapabilityHandle, InterfaceError> {
        let validation_time = if bearer == self.expired_bearer {
            now + Duration::hours(1)
        } else {
            now
        };
        self.store.authenticate(bearer, validation_time).await
    }

    async fn revoke(&self, principal: &PrincipalId) -> Result<(), InterfaceError> {
        self.store.revoke(principal).await
    }
}

fn submit_request(token: &str, duplicate_authorization: bool) -> Request<Body> {
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: types::SessionId::new(),
        page_id: None,
        deadline: Utc::now() + Duration::minutes(1),
        command: RuntimeCommand::Primitive(PrimitiveCommand::ListPages(types::ListPagesCommand)),
    };
    let deadline = (Utc::now() + Duration::minutes(2)).to_rfc3339_opts(SecondsFormat::Millis, true);
    let mut request = Request::post("/v1/commands")
        .header("authorization", format!("Bearer {token}"))
        .header("x-interface-version", CURRENT_INTERFACE_VERSION)
        .header("x-correlation-id", "10000000-0000-0000-0000-000000000099")
        .header("x-deadline", deadline)
        .header("idempotency-key", "authentication-test")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&envelope).unwrap()))
        .unwrap();
    if duplicate_authorization {
        request
            .headers_mut()
            .append("authorization", "Bearer duplicate".parse().unwrap());
    }
    request
}

#[tokio::test]
async fn every_v1_route_fails_closed_without_bearer_authentication() {
    let authority = AuthorityStore::in_memory();
    for request in protected_requests() {
        let response = app(authority.clone(), Arc::new(Mutex::new(None)))
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()[WWW_AUTHENTICATE], "Bearer");
        let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["error"]["code"],
            "authenticationFailed"
        );
    }
}

#[tokio::test]
async fn insufficient_scope_is_forbidden_before_runtime_dispatch() {
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000001")),
            [Capability::SessionRead],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let observed = Arc::new(Mutex::new(None));

    let response = app(authority, observed.clone())
        .oneshot(submit_request(&token, false))
        .await
        .unwrap();

    let status = response.status();
    let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(json["error"]["requiredCapability"], "browser:mutate");
    assert_eq!(
        observed
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .submit_dispatch_count(),
        0
    );
}

#[tokio::test]
async fn duplicate_authorization_is_rejected_before_authentication() {
    let response = app(AuthorityStore::in_memory(), Arc::new(Mutex::new(None)))
        .oneshot(submit_request("not-a-credential", true))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(response.headers()[WWW_AUTHENTICATE], "Bearer");
}

#[tokio::test]
async fn authentication_result_precedes_all_context_header_validation() {
    let authority = AuthorityStore::in_memory();
    let valid = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000041")),
            [Capability::BrowserMutate],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let revoked_principal = PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000042"));
    let revoked = authority
        .issue(
            revoked_principal.clone(),
            [Capability::BrowserMutate],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    authority.revoke(&revoked_principal).await.unwrap();
    let expired = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000043")),
            [Capability::BrowserMutate],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let matrix_authority = Arc::new(ExpiredBearerAuthority {
        store: authority.clone(),
        expired_bearer: expired.clone(),
    });

    for bearer in [
        "wrong-authentication-bearer-00000001",
        revoked.as_str(),
        expired.as_str(),
    ] {
        let mut request = submit_request(bearer, false);
        request
            .headers_mut()
            .insert("x-correlation-id", "malformed".parse().unwrap());
        request
            .headers_mut()
            .insert("x-interface-version", "malformed".parse().unwrap());
        request
            .headers_mut()
            .insert("x-deadline", "malformed".parse().unwrap());
        let response = app_with_authority(matrix_authority.clone(), Arc::new(Mutex::new(None)))
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{bearer}");
        assert_eq!(response.headers()[WWW_AUTHENTICATE], "Bearer");
    }

    let mut malformed_context = submit_request(&valid, false);
    malformed_context
        .headers_mut()
        .insert("x-correlation-id", "malformed".parse().unwrap());
    assert_eq!(
        app_with_authority(matrix_authority.clone(), Arc::new(Mutex::new(None)))
            .oneshot(malformed_context)
            .await
            .unwrap()
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );

    let mut expired_deadline = submit_request(&valid, false);
    expired_deadline.headers_mut().insert(
        "x-deadline",
        (Utc::now() - Duration::seconds(1))
            .to_rfc3339_opts(SecondsFormat::Millis, true)
            .parse()
            .unwrap(),
    );
    assert_eq!(
        app_with_authority(matrix_authority, Arc::new(Mutex::new(None)))
            .oneshot(expired_deadline)
            .await
            .unwrap()
            .status(),
        StatusCode::REQUEST_TIMEOUT
    );
}

#[tokio::test]
async fn startup_enrollment_accepts_only_the_explicit_bearer_and_honors_revocation() {
    let bearer = "explicit-startup-bearer-00000001".to_owned();
    let principal = PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000077"));
    let authority = EnrolledAuthority::enroll(
        StartupCredential::new(
            bearer.clone(),
            principal.clone(),
            vec![Capability::SessionRead],
            Utc::now() + Duration::minutes(5),
        )
        .unwrap(),
        1,
    )
    .await
    .unwrap();

    let handle = authority.authenticate(&bearer, Utc::now()).await.unwrap();
    let context = handle.context(Utc::now() + Duration::minutes(2), None);
    assert!(authority
        .authenticate("different-startup-bearer-000000", Utc::now())
        .await
        .is_err());
    assert!(!format!("{authority:?}").contains(&bearer));

    authority.revoke(&principal).await.unwrap();
    assert!(authority.authenticate(&bearer, Utc::now()).await.is_err());
    assert!(AuthorizationGuard::new(handle)
        .authorize(&context, InterfaceOperation::RuntimeInfo)
        .is_err());
}

#[tokio::test]
async fn startup_bearer_limits_fit_the_authorization_header_exactly() {
    let principal = PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000078"));
    let expires = Utc::now() + Duration::minutes(5);
    let max = "x".repeat(505);

    let credential = StartupCredential::new(
        max.clone(),
        principal.clone(),
        vec![Capability::SessionRead],
        expires,
    )
    .unwrap();
    assert!(!format!("{credential:?}").contains(&max));
    let authority = EnrolledAuthority::enroll(credential, 1).await.unwrap();
    let runtime = RuntimeService::default();
    let app = router(AppState::new(
        Arc::new(authority),
        move |handle| {
            Arc::new(AuthenticatedRuntime::new(runtime.clone(), handle))
                as Arc<dyn RuntimeInterface>
        },
        InterfaceConfig::default(),
    ));
    let response = app
        .oneshot(
            Request::get("/v1/runtime")
                .header("authorization", format!("Bearer {max}"))
                .header("x-interface-version", CURRENT_INTERFACE_VERSION)
                .header("x-correlation-id", "10000000-0000-0000-0000-000000000079")
                .header(
                    "x-deadline",
                    (Utc::now() + Duration::minutes(2))
                        .to_rfc3339_opts(SecondsFormat::Millis, true),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(StartupCredential::new(
        "x".repeat(506),
        principal.clone(),
        vec![Capability::SessionRead],
        expires,
    )
    .is_err());
    assert!(StartupCredential::new(
        "x".repeat(31),
        principal,
        vec![Capability::SessionRead],
        expires,
    )
    .is_err());
}
