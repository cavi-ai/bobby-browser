mod support;

use std::sync::Arc;

use cdp_gateway::{CdpConnection, IdentifierFamily, MethodRegistry, RuntimeGeneration};
use chrono::{Duration, Utc};
use interface_core::AuthorityStore;
use types::{Capability, PrincipalId};

#[tokio::test]
async fn all_identifier_families_are_connection_state_and_generation_events_precede_removal() {
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [Capability::SessionRead, Capability::FileDownload],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap();
    let connection = CdpConnection::new(
        authority.verify(&token.expose_once()).await.unwrap(),
        Arc::new(support::StaticRuntime { sessions: vec![] }),
        MethodRegistry::compiled(),
    );
    for family in IdentifierFamily::ALL {
        let opaque = connection
            .bind_identifier(
                family,
                "runtime-session",
                &format!("internal-{family:?}"),
                RuntimeGeneration(1),
            )
            .await;
        assert!(connection
            .resolve_identifier(family, &opaque)
            .await
            .is_some());
    }
    connection
        .replace_generation("runtime-session", RuntimeGeneration(2))
        .await
        .unwrap();
    let methods = connection
        .drain_events()
        .await
        .into_iter()
        .map(|event| event.method)
        .collect::<Vec<_>>();
    assert_eq!(
        methods,
        [
            "Network.loadingFailed",
            "Browser.downloadProgress",
            "Runtime.executionContextDestroyed",
            "Page.frameDetached",
            "Target.detachedFromTarget",
            "Target.targetDestroyed",
            "Target.browserContextDestroyed",
        ]
    );
}

#[tokio::test]
async fn generation_replacement_keeps_mappings_when_teardown_cannot_be_queued() {
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [Capability::SessionRead, Capability::FileDownload],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap();
    let connection = CdpConnection::new(
        authority.verify(&token.expose_once()).await.unwrap(),
        Arc::new(support::StaticRuntime { sessions: vec![] }),
        MethodRegistry::compiled(),
    );
    let target = connection
        .bind_identifier(
            IdentifierFamily::Target,
            "runtime-session",
            "page",
            RuntimeGeneration(1),
        )
        .await;
    for _ in 0..cdp_gateway::MAX_QUEUED_EVENTS {
        connection
            .queue_event(cdp_gateway::CdpEvent {
                method: "Target.targetDestroyed".into(),
                params: serde_json::json!({"targetId":"existing"}),
                session_id: None,
            })
            .await
            .unwrap();
    }
    assert!(connection
        .replace_generation("runtime-session", RuntimeGeneration(2))
        .await
        .is_err());
    assert_eq!(
        connection.resolve_target(&target).await.as_deref(),
        Some("page")
    );
}
