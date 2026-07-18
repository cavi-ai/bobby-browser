mod support;

use std::sync::Arc;

use cdp_gateway::{CdpGateway, CdpRequest, MethodRegistry};
use chrono::{Duration, Utc};
use interface_core::AuthorityStore;
use serde_json::json;
use types::{Capability, PageId, PrincipalId, SessionId, SessionState};

#[tokio::test]
async fn discovery_and_connection_share_stable_per_page_opaque_targets() {
    let now = Utc::now();
    let page_a = PageId::new();
    let page_b = PageId::new();
    let runtime = Arc::new(support::StaticRuntime {
        sessions: vec![
            SessionState {
                id: SessionId::new(),
                profile: "empty".into(),
                proxy: None,
                page_ids: vec![],
                created_at: now,
                last_used_at: now,
            },
            SessionState {
                id: SessionId::new(),
                profile: "two-pages".into(),
                proxy: None,
                page_ids: vec![page_a, page_b],
                created_at: now,
                last_used_at: now,
            },
        ],
    });
    let authority = Arc::new(AuthorityStore::in_memory());
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [Capability::SessionRead],
            now + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let gateway = CdpGateway::new(
        authority,
        runtime,
        MethodRegistry::compiled(),
        "ws://localhost",
    );
    let first = gateway.list(Some(&token)).await.unwrap();
    let second = gateway.list(Some(&token)).await.unwrap();
    assert_eq!(first.len(), 2);
    assert_eq!(
        first.iter().map(|target| &target.id).collect::<Vec<_>>(),
        second.iter().map(|target| &target.id).collect::<Vec<_>>()
    );
    let version = gateway.version(Some(&token)).await.unwrap();
    let connection = gateway
        .upgrade(
            version
                .web_socket_debugger_url
                .strip_prefix("ws://localhost")
                .unwrap(),
            Some(&token),
        )
        .await
        .unwrap();
    let response = connection
        .dispatch(CdpRequest::new(1, "Target.getTargets", json!({})))
        .await;
    let ids = response.result().unwrap()["targetInfos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|target| target["targetId"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        first
            .iter()
            .map(|target| target.id.as_str())
            .collect::<Vec<_>>()
    );
    for id in ids {
        assert!(connection.resolve_target(id).await.is_some());
    }
}
