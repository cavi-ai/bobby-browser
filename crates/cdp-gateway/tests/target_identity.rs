mod support;

use std::sync::Arc;

use cdp_gateway::{CdpConnection, CdpGateway, CdpRequest, MethodRegistry};
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
                execution_policy: types::ExecutionPolicy::default(),
                zigzagzig: false,
            },
            SessionState {
                id: SessionId::new(),
                profile: "two-pages".into(),
                proxy: None,
                page_ids: vec![page_a, page_b],
                created_at: now,
                last_used_at: now,
                execution_policy: types::ExecutionPolicy::default(),
                zigzagzig: false,
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

#[tokio::test]
async fn json_list_auto_session_returns_a_page_before_websocket() {
    let now = Utc::now();
    let runtime = support::AutoSessionRuntime::empty();
    let authority = Arc::new(AuthorityStore::in_memory());
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [
                Capability::SessionRead,
                Capability::SessionWrite,
                Capability::PageWrite,
            ],
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

    let listed = gateway.list(Some(&token)).await.unwrap();

    assert_eq!(listed.len(), 1);
}

#[tokio::test]
async fn page_target_info_uses_verified_navigation_title() {
    let now = Utc::now();
    let runtime = Arc::new(support::NavigatingRuntime {
        sessions: vec![SessionState {
            id: SessionId::new(),
            profile: "p".into(),
            proxy: None,
            page_ids: vec![PageId::new()],
            created_at: now,
            last_used_at: now,
            execution_policy: types::ExecutionPolicy::default(),
            zigzagzig: false,
        }],
        title: "Bobby agent-benchmark fixture".into(),
    });
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [
                Capability::SessionRead,
                Capability::PageRead,
                Capability::PageWrite,
            ],
            now + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let connection = CdpConnection::new(
        authority.verify(&token).await.unwrap(),
        runtime,
        MethodRegistry::compiled(),
    );
    let attached = connection
        .dispatch(CdpRequest::new(
            1,
            "Target.setAutoAttach",
            json!({
                "autoAttach": true,
                "waitForDebuggerOnStart": true,
                "flatten": true
            }),
        ))
        .await;
    assert!(attached.error().is_none(), "{:?}", attached.error());
    let event = connection
        .drain_events()
        .await
        .into_iter()
        .find(|event| event.method == "Target.attachedToTarget")
        .expect("page CDP session");
    let session_id = event.params["sessionId"].as_str().unwrap().to_owned();
    let target_id = event.params["targetInfo"]["targetId"]
        .as_str()
        .unwrap()
        .to_owned();

    let mut navigate =
        CdpRequest::new(2, "Page.navigate", json!({"url": "http://127.0.0.1:8766/"}));
    navigate.session_id = Some(session_id);
    let navigated = connection.dispatch(navigate).await;
    assert!(navigated.error().is_none(), "{:?}", navigated.error());

    let info = connection
        .dispatch(CdpRequest::new(
            3,
            "Target.getTargetInfo",
            json!({"targetId": target_id}),
        ))
        .await;
    let target = &info.result().unwrap()["targetInfo"];
    assert_eq!(target["title"], "Bobby agent-benchmark fixture");
    assert_eq!(target["url"], "http://127.0.0.1:8766/");
}
