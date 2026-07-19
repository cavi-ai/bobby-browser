mod support;

use std::sync::Arc;

use cdp_gateway::{
    CdpConnection, CdpErrorCode, CdpRequest, MethodRegistry, MAX_IN_FLIGHT_REQUESTS,
};
use chrono::{Duration, Utc};
use interface_core::AuthorityStore;
use serde_json::json;
use types::{Capability, PrincipalId};

#[tokio::test]
async fn connection_runs_requests_concurrently_and_rejects_before_spawning_over_bound() {
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [Capability::SessionRead],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap();
    let runtime = support::BlockingRuntime::new();
    let connection = Arc::new(CdpConnection::new(
        authority.verify(&token.expose_once()).await.unwrap(),
        runtime.clone(),
        MethodRegistry::compiled(),
    ));
    let mut tasks = Vec::new();
    for id in 1..=MAX_IN_FLIGHT_REQUESTS {
        let connection = connection.clone();
        tasks.push(tokio::spawn(async move {
            connection
                .dispatch(CdpRequest::new(id as u64, "Target.getTargets", json!({})))
                .await
        }));
    }
    runtime.wait_for_active(MAX_IN_FLIGHT_REQUESTS).await;
    assert_eq!(runtime.peak(), MAX_IN_FLIGHT_REQUESTS);
    let overflow = connection
        .dispatch(CdpRequest::new(999, "Target.getTargets", json!({})))
        .await;
    assert_eq!(
        overflow.error().unwrap().code,
        CdpErrorCode::RuntimeFailure as i32
    );
    runtime.release_all();
    for task in tasks {
        assert!(task.await.unwrap().error().is_none());
    }
}
