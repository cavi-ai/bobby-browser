//! End-to-end persistence: a principal issued through the real `POST /v1/principals`
//! HTTP route must still authenticate after the router (and its authority) is dropped
//! and rebuilt over the same on-disk authority file. This guards the wiring between the
//! route handler and the persistent authority layer — a regression that decoupled them
//! (e.g. issuing through a non-persistent authority in production only) would pass every
//! in-memory test but fail here.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use broker::testing::{app_with_admin_and_quota_at, context_headers, issue_bearer};
use config::InterfaceConfig;
use tower::ServiceExt;
use uuid::uuid;

async fn sessions_status(app: &axum::Router, bearer: &str) -> StatusCode {
    let request = context_headers(Request::get("/v1/sessions"), bearer)
        .body(Body::empty())
        .expect("sessions request builds");
    app.clone()
        .oneshot(request)
        .await
        .expect("router answers")
        .status()
}

#[tokio::test]
async fn http_issued_principal_survives_a_restart_over_the_same_authority_file() {
    let authority_path =
        std::env::temp_dir().join(format!("bobby-persist-http-{}.json", uuid::Uuid::new_v4()));
    let default_quota = InterfaceConfig::default().max_in_flight_per_principal;
    let principal = uuid!("10000000-0000-0000-0000-0000000000e2");

    // First "process": issue a team principal through the real HTTP route.
    let team_bearer = {
        let (app, _authority, admin_bearer) =
            app_with_admin_and_quota_at(8, default_quota, authority_path.clone()).await;
        let bearer = issue_bearer(&app, &admin_bearer, principal, &["session:read"]).await;
        // Sanity: the freshly issued bearer authenticates in this process.
        assert_eq!(sessions_status(&app, &bearer).await, StatusCode::OK);
        bearer
        // `app` and its `PersistentAuthority` drop here — the on-disk file is all that
        // survives into the next "process".
    };

    // Second "process": brand-new router + fresh EnrolledAuthority, same authority file.
    // Nothing is shared in memory — only the persisted hash record.
    let (app, _authority, _admin_bearer) =
        app_with_admin_and_quota_at(8, default_quota, authority_path.clone()).await;
    assert_eq!(
        sessions_status(&app, &team_bearer).await,
        StatusCode::OK,
        "HTTP-issued bearer must still authenticate after a restart over the same authority file"
    );

    let _ = std::fs::remove_file(&authority_path);
}

#[tokio::test]
async fn http_revocation_survives_a_restart_over_the_same_authority_file() {
    let authority_path = std::env::temp_dir().join(format!(
        "bobby-persist-http-revoke-{}.json",
        uuid::Uuid::new_v4()
    ));
    let default_quota = InterfaceConfig::default().max_in_flight_per_principal;
    let principal = uuid!("10000000-0000-0000-0000-0000000000e3");

    let team_bearer = {
        let (app, _authority, admin_bearer) =
            app_with_admin_and_quota_at(8, default_quota, authority_path.clone()).await;
        let bearer = issue_bearer(&app, &admin_bearer, principal, &["session:read"]).await;
        // Revoke through the real HTTP route.
        let revoke = context_headers(
            Request::delete(format!("/v1/principals/{principal}")),
            &admin_bearer,
        )
        .body(Body::empty())
        .expect("revoke request builds");
        let status = app
            .clone()
            .oneshot(revoke)
            .await
            .expect("router answers")
            .status();
        assert_eq!(status, StatusCode::NO_CONTENT);
        bearer
    };

    let (app, _authority, _admin_bearer) =
        app_with_admin_and_quota_at(8, default_quota, authority_path.clone()).await;
    assert_eq!(
        sessions_status(&app, &team_bearer).await,
        StatusCode::UNAUTHORIZED,
        "an HTTP-revoked principal must stay revoked after a restart over the same authority file"
    );

    let _ = std::fs::remove_file(&authority_path);
}
