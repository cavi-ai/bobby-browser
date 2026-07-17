use std::sync::Arc;

use artifact_store::ArtifactStore;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use broker::{router, AppState, ArtifactCatalog};
use chrono::{Duration, SecondsFormat, Utc};
use config::InterfaceConfig;
use interface_core::{
    ArtifactOwnershipLimits, ArtifactReader, AuthorityStore, Event, EventStore, RuntimeInterface,
    SessionOwnershipRecorder, SessionOwnershipRegistry,
};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::json;
use tempfile::TempDir;
use tower::ServiceExt;
use types::{Capability, PageId, PrincipalId, SessionId, CURRENT_INTERFACE_VERSION};
use uuid::Uuid;

fn authorized(method: &str, uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("x-interface-version", CURRENT_INTERFACE_VERSION)
        .header("x-correlation-id", "10000000-0000-0000-0000-000000000021")
        .header(
            "x-deadline",
            (Utc::now() + Duration::minutes(2)).to_rfc3339_opts(SecondsFormat::Millis, true),
        )
        .body(Body::empty())
        .unwrap()
}

fn app(
    authority: AuthorityStore,
    interface: InterfaceConfig,
    events: EventStore,
    artifacts: ArtifactCatalog,
) -> axum::Router {
    let runtime = RuntimeService::default();
    router(
        AppState::new(
            Arc::new(authority),
            move |handle| {
                Arc::new(AuthenticatedRuntime::new(runtime.clone(), handle))
                    as Arc<dyn RuntimeInterface>
            },
            interface,
        )
        .with_boundaries(events, artifacts),
    )
}

async fn issue(
    authority: &AuthorityStore,
    principal: u128,
    capabilities: impl IntoIterator<Item = Capability>,
) -> String {
    authority
        .issue(
            PrincipalId::from_uuid(Uuid::from_u128(principal)),
            capabilities,
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once()
}

#[tokio::test]
async fn event_gap_and_query_bounds_survive_the_authenticated_route() {
    let authority = AuthorityStore::in_memory();
    let token = issue(&authority, 1, [Capability::SessionRead]).await;
    let events = EventStore::new(2);
    for sequence in 1..=3 {
        events
            .append(Event::new("sequence", json!({ "sequence": sequence })))
            .await;
    }
    let interface = InterfaceConfig {
        max_event_batch: 2,
        max_event_retention: 2,
        ..InterfaceConfig::default()
    };
    let app = app(authority, interface, events, ArtifactCatalog::default());

    let gap = app
        .clone()
        .oneshot(authorized("GET", "/v1/events?after=0&limit=2", &token))
        .await
        .unwrap();
    assert_eq!(gap.status(), StatusCode::CONFLICT);
    let body = to_bytes(gap.into_body(), 16 * 1024).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["reason"], "historyLost");
    assert_eq!(json["earliestAvailable"], 2);

    for query in [
        "/v1/events?after=1&limit=3",
        "/v1/events?after=1&after=2&limit=1",
        "/v1/events?after=1&limit=1&owner=caller",
    ] {
        let response = app
            .clone()
            .oneshot(authorized("GET", query, &token))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        if query.contains("after=1&after=2") {
            let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                json["error"]["correlationId"],
                "10000000-0000-0000-0000-000000000021"
            );
        }
    }
}

struct ArtifactFixture {
    _root: TempDir,
    app: axum::Router,
    owner_token: String,
    other_token: String,
    artifact_id: String,
}

async fn artifact_fixture() -> ArtifactFixture {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(root.path(), 4096, 4096);
    let authority = AuthorityStore::in_memory();
    let owner_token = issue(
        &authority,
        10,
        [Capability::ArtifactRead, Capability::ArtifactCapture],
    )
    .await;
    let other_token = issue(&authority, 11, [Capability::ArtifactRead]).await;
    let owner_handle = authority.verify(&owner_token).await.unwrap();
    let owner_context = owner_handle.context(Utc::now() + Duration::minutes(2), None);
    let session = SessionId::new();
    let (ownership, recorder): (_, SessionOwnershipRecorder) = SessionOwnershipRegistry::bounded(2);
    recorder
        .record_authenticated_session(owner_context.principal_id.clone(), session.clone())
        .unwrap();
    let reader = ArtifactReader::new(
        store.clone(),
        ownership,
        4096,
        ArtifactOwnershipLimits {
            max_records: 4,
            max_bytes: 16 * 1024,
        },
    )
    .unwrap();
    let record = store
        .put(
            &session,
            &PageId::new(),
            "application/octet-stream",
            "bin",
            b"bounded artifact bytes",
            4096,
        )
        .await
        .unwrap();
    let reference = reader
        .register(&owner_handle, &owner_context, &session, &record)
        .await
        .unwrap();
    let artifact_id = reference.artifact_id().to_owned();
    let catalog = ArtifactCatalog::new(reader.clone(), 1);
    catalog
        .register_trusted(session.clone(), reference)
        .await
        .unwrap();

    let second_record = store
        .put(
            &session,
            &PageId::new(),
            "application/octet-stream",
            "bin",
            b"second artifact",
            4096,
        )
        .await
        .unwrap();
    let second_reference = reader
        .register(&owner_handle, &owner_context, &session, &second_record)
        .await
        .unwrap();
    assert!(catalog
        .register_trusted(session, second_reference)
        .await
        .is_err());

    ArtifactFixture {
        _root: root,
        app: app(
            authority,
            InterfaceConfig::default(),
            EventStore::new(2),
            catalog,
        ),
        owner_token,
        other_token,
        artifact_id,
    }
}

#[tokio::test]
async fn artifact_route_is_bounded_range_free_and_principal_isolated() {
    let fixture = artifact_fixture().await;
    let uri = format!("/v1/artifacts/{}", fixture.artifact_id);
    let response = fixture
        .app
        .clone()
        .oneshot(authorized("GET", &uri, &fixture.owner_token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "application/octet-stream"
    );
    assert_eq!(response.headers()["content-length"], "22");
    let body = to_bytes(response.into_body(), 4096).await.unwrap();
    assert_eq!(body.as_ref(), b"bounded artifact bytes");

    let denied = fixture
        .app
        .clone()
        .oneshot(authorized("GET", &uri, &fixture.other_token))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    let body = to_bytes(denied.into_body(), 16 * 1024).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["code"], "artifactDenied");

    let mut range = authorized("GET", &uri, &fixture.owner_token);
    range
        .headers_mut()
        .insert("range", "bytes=0-3".parse().unwrap());
    assert_eq!(
        fixture.app.clone().oneshot(range).await.unwrap().status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );

    let caller_owned = format!("{uri}?sessionId={}", SessionId::new().0);
    assert_eq!(
        fixture
            .app
            .oneshot(authorized("GET", &caller_owned, &fixture.owner_token))
            .await
            .unwrap()
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
}
