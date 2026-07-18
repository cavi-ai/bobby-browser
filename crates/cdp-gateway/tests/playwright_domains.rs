mod support;

use std::sync::Arc;

use cdp_gateway::{CdpConnection, CdpErrorCode, CdpRequest, MethodRegistry};
use chrono::{Duration, Utc};
use interface_core::AuthorityStore;
use serde_json::json;
use types::{Capability, PageId, PrincipalId, SessionId, SessionState};

async fn connection(capabilities: impl IntoIterator<Item = Capability>) -> CdpConnection {
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            capabilities,
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let now = Utc::now();
    CdpConnection::new(
        authority.verify(&token).await.unwrap(),
        Arc::new(support::StaticRuntime {
            sessions: vec![SessionState {
                id: SessionId::new(),
                profile: "p".into(),
                proxy: None,
                page_ids: vec![PageId::new()],
                created_at: now,
                last_used_at: now,
            }],
        }),
        MethodRegistry::compiled(),
    )
}

#[tokio::test]
async fn auto_attach_emits_existing_target_only_after_enable() {
    let connection = connection([Capability::SessionRead]).await;
    assert!(connection.next_event().await.is_none());
    let response = connection
        .dispatch(CdpRequest::new(
            1,
            "Target.setAutoAttach",
            json!({
                "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true
            }),
        ))
        .await;
    assert!(response.error().is_none(), "{:?}", response.error());
    let event = connection.next_event().await.expect("attached event");
    assert_eq!(event.method, "Target.attachedToTarget");
    assert!(event.params["sessionId"]
        .as_str()
        .is_some_and(|id| !id.is_empty()));
    assert!(event.params["targetInfo"]["browserContextId"]
        .as_str()
        .is_some_and(|id| !id.is_empty()));
    assert_eq!(event.params["targetInfo"]["attached"], true);
    assert_eq!(event.params["waitingForDebugger"], true);
}

#[tokio::test]
async fn browser_target_info_is_available_during_bootstrap() {
    let response = connection([Capability::SessionRead])
        .await
        .dispatch(CdpRequest::new(1, "Target.getTargetInfo", json!({})))
        .await;
    assert_eq!(response.result().unwrap()["targetInfo"]["type"], "browser");
    assert!(response.result().unwrap()["targetInfo"]["targetId"]
        .as_str()
        .is_some_and(|id| !id.is_empty()));
}

#[tokio::test]
async fn page_and_runtime_bootstrap_are_bounded_and_runtime_enable_orders_context_event() {
    let connection = connection([Capability::PageRead, Capability::JavascriptEvaluate]).await;
    assert!(connection.next_event().await.is_none());
    let frame = connection
        .dispatch(CdpRequest::new(1, "Page.getFrameTree", json!({})))
        .await;
    assert_eq!(
        frame.result().unwrap()["frameTree"]["frame"]["url"],
        "about:blank"
    );
    assert!(connection.next_event().await.is_none());
    let enabled = connection
        .dispatch(CdpRequest::new(2, "Runtime.enable", json!({})))
        .await;
    assert!(enabled.error().is_none());
    let event = connection
        .next_event()
        .await
        .expect("execution context event");
    assert_eq!(event.method, "Runtime.executionContextCreated");
    assert_eq!(event.params["context"]["auxData"]["isDefault"], true);
}

#[tokio::test]
async fn nested_auto_attach_does_not_duplicate_page_targets() {
    let connection = connection([Capability::SessionRead]).await;
    let mut request = CdpRequest::new(
        1,
        "Target.setAutoAttach",
        json!({
            "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true
        }),
    );
    request.session_id = Some("opaque-session".into());
    let response = connection.dispatch(request).await;
    assert!(response.error().is_none());
    assert!(connection.next_event().await.is_none());
}

#[tokio::test]
async fn auto_attach_rejects_malformed_params_and_missing_capability() {
    let malformed = connection([Capability::SessionRead])
        .await
        .dispatch(CdpRequest::new(
            1,
            "Target.setAutoAttach",
            json!({"autoAttach": "yes"}),
        ))
        .await;
    assert_eq!(
        malformed.error().unwrap().code,
        CdpErrorCode::InvalidParams as i32
    );

    let denied = connection([])
        .await
        .dispatch(CdpRequest::new(
            2,
            "Target.setAutoAttach",
            json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true}),
        ))
        .await;
    assert_eq!(
        denied.error().unwrap().code,
        CdpErrorCode::RuntimeFailure as i32
    );
}

#[tokio::test]
async fn download_behavior_is_bounded_and_capability_checked() {
    let allowed = connection([Capability::FileDownload])
        .await
        .dispatch(CdpRequest::new(
            1,
            "Browser.setDownloadBehavior",
            json!({"behavior":"allowAndName", "eventsEnabled":true}),
        ))
        .await;
    assert!(allowed.error().is_none());
    let denied = connection([Capability::SessionRead])
        .await
        .dispatch(CdpRequest::new(
            2,
            "Browser.setDownloadBehavior",
            json!({"behavior":"allowAndName", "eventsEnabled":true}),
        ))
        .await;
    assert_eq!(
        denied.error().unwrap().code,
        CdpErrorCode::RuntimeFailure as i32
    );
}
