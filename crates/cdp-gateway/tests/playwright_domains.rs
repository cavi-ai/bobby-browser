mod support;

use std::sync::Arc;

use cdp_gateway::{CdpConnection, CdpErrorCode, CdpEvent, CdpRequest, MethodRegistry};
use chrono::{Duration, Utc};
use interface_core::AuthorityStore;
use serde_json::json;
use types::{Capability, PageId, PrincipalId, SessionId, SessionState};

const MAX_RECORDED_PROTOCOL_ITEMS: usize = 16;

#[derive(Debug)]
struct ProtocolRecorder(Vec<serde_json::Value>);

impl ProtocolRecorder {
    fn record(&mut self, value: serde_json::Value) {
        assert!(
            self.0.len() < MAX_RECORDED_PROTOCOL_ITEMS,
            "protocol recording exceeded its hard bound"
        );
        self.0.push(value);
    }
}

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
                execution_policy: types::ExecutionPolicy::default(),
            }],
        }),
        MethodRegistry::compiled(),
    )
}

async fn page_creating_connection() -> CdpConnection {
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [Capability::SessionRead, Capability::PageWrite],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let now = Utc::now();
    CdpConnection::new(
        authority.verify(&token).await.unwrap(),
        Arc::new(support::PageCreatingRuntime {
            session: SessionState {
                id: SessionId::new(),
                profile: "p".into(),
                proxy: None,
                page_ids: vec![],
                created_at: now,
                last_used_at: now,
                execution_policy: types::ExecutionPolicy::default(),
            },
        }),
        MethodRegistry::compiled(),
    )
}

#[tokio::test]
async fn target_event_admission_rejects_worker_families_and_allows_popup_pages() {
    let connection = connection([Capability::SessionRead]).await;

    for target_type in ["worker", "service_worker"] {
        connection
            .queue_event(CdpEvent {
                method: "Target.targetCreated".into(),
                params: json!({"targetInfo": {
                    "targetId": format!("attack-{target_type}"),
                    "type": target_type,
                    "title": "untrusted target",
                    "url": "https://example.invalid/",
                    "attached": false,
                    "canAccessOpener": false
                }}),
                session_id: None,
            })
            .await
            .unwrap();
    }
    assert!(
        connection.drain_events().await.is_empty(),
        "worker and service-worker targets must not cross the CDP admission boundary"
    );

    connection
        .queue_event(CdpEvent {
            method: "Target.targetCreated".into(),
            params: json!({"targetInfo": {
                "targetId": "popup-page",
                "type": "page",
                "title": "popup",
                "url": "https://example.invalid/popup",
                "attached": false,
                "canAccessOpener": true,
                "openerId": "parent-page"
            }}),
            session_id: None,
        })
        .await
        .unwrap();
    let popup = connection.drain_events().await;
    assert_eq!(popup.len(), 1);
    assert_eq!(popup[0].params["targetInfo"]["type"], "page");
    assert_eq!(popup[0].params["targetInfo"]["openerId"], "parent-page");
    println!("AUTOMATION_RUNTIME_SECURITY_PROOF:v1:cdp-target-context-policy");
}

#[tokio::test]
async fn client_initialization_accepts_any_pinned_puppeteer_utility_world_version() {
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [
                Capability::SessionRead,
                Capability::PageWrite,
                Capability::JavascriptEvaluate,
            ],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let now = Utc::now();
    let connection = CdpConnection::new(
        authority.verify(&token).await.unwrap(),
        Arc::new(support::PageCreatingRuntime {
            session: SessionState {
                id: SessionId::new(),
                profile: "p".into(),
                proxy: None,
                page_ids: vec![],
                created_at: now,
                last_used_at: now,
                execution_policy: types::ExecutionPolicy::default(),
            },
        }),
        MethodRegistry::compiled(),
    );

    for world in [
        "__puppeteer_utility_world__25.4.0",
        "__puppeteer_utility_world__25.5.0",
        "__puppeteer_utility_world__26.0.0",
    ] {
        let response = connection
            .dispatch(CdpRequest::new(
                1,
                "Page.addScriptToEvaluateOnNewDocument",
                json!({"source":"//# sourceURL=pptr:internal","worldName":world}),
            ))
            .await;
        let value = serde_json::to_value(response).unwrap();
        assert!(
            value["result"]["identifier"].is_string(),
            "{world}: {value}"
        );
    }

    for params in [
        json!({"source":"//# sourceURL=pptr:internal","worldName":"__puppeteer_utility_world__"}),
        json!({"source":"//# sourceURL=pptr:internal","worldName":"__puppeteer_utility_world__25..0"}),
        json!({"source":"//# sourceURL=pptr:internal","worldName":"__puppeteer_utility_world__25.4.0-evil"}),
        json!({"source":"fetch('http://attacker')","worldName":"__puppeteer_utility_world__25.4.0"}),
    ] {
        let response = connection
            .dispatch(CdpRequest::new(
                2,
                "Page.addScriptToEvaluateOnNewDocument",
                params.clone(),
            ))
            .await;
        let value = serde_json::to_value(response).unwrap();
        assert_eq!(
            value["error"]["code"],
            CdpErrorCode::InvalidParams as i32,
            "{params}: {value}"
        );
    }
}

#[tokio::test]
async fn creating_a_target_without_a_runtime_session_names_where_sessions_come_from() {
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [Capability::SessionRead, Capability::PageWrite],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let connection = CdpConnection::new(
        authority.verify(&token).await.unwrap(),
        Arc::new(support::StaticRuntime { sessions: vec![] }),
        MethodRegistry::compiled(),
    );

    let response = connection
        .dispatch(CdpRequest::new(
            1,
            "Target.createTarget",
            json!({"url":"about:blank"}),
        ))
        .await;

    let error = serde_json::to_value(response).unwrap()["error"]["message"]
        .as_str()
        .expect("failure carries a message")
        .to_owned();
    assert!(error.contains("cannot create one"), "{error}");
    assert!(error.contains("/v1/sessions"), "{error}");
    assert!(error.contains("page_open"), "{error}");
}

#[tokio::test]
async fn puppeteer_target_manager_receives_tab_then_child_page_with_parent_routing() {
    let connection = page_creating_connection().await;
    let mut recorder = ProtocolRecorder(Vec::new());
    for request in [
        CdpRequest::new(
            1,
            "Target.setDiscoverTargets",
            json!({"discover":true,"filter":[{}]}),
        ),
        CdpRequest::new(
            2,
            "Target.setAutoAttach",
            json!({
                "autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true,
                "filter":[{"type":"page","exclude":true},{}]
            }),
        ),
        CdpRequest::new(3, "Target.createTarget", json!({"url":"about:blank"})),
    ] {
        recorder.record(json!({"request":{"id":request.id,"method":request.method,"sessionId":request.session_id}}));
        let response = connection.dispatch(request).await;
        recorder.record(serde_json::to_value(response).unwrap());
    }
    let events = connection.drain_events().await;
    assert_eq!(
        events
            .iter()
            .map(|event| event.method.as_str())
            .collect::<Vec<_>>(),
        [
            "Target.targetCreated",
            "Target.targetCreated",
            "Target.attachedToTarget"
        ]
    );
    let tab = events.last().unwrap();
    assert_eq!(tab.params["targetInfo"]["type"], "tab");
    let tab_session = tab.params["sessionId"].as_str().unwrap().to_owned();
    let tab_target = tab.params["targetInfo"]["targetId"].clone();
    for event in events {
        recorder.record(serde_json::to_value(event).unwrap());
    }

    let mut nested = CdpRequest::new(
        4,
        "Target.setAutoAttach",
        json!({
            "autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true,"filter":[{}]
        }),
    );
    nested.session_id = Some(tab_session.clone());
    recorder.record(
        json!({"request":{"id":nested.id,"method":nested.method,"sessionId":nested.session_id}}),
    );
    let response = connection.dispatch(nested).await;
    assert!(response.error().is_none());
    recorder.record(serde_json::to_value(response).unwrap());
    let child = connection
        .next_event()
        .await
        .expect("nested page attachment");
    recorder.record(serde_json::to_value(&child).unwrap());
    assert_eq!(child.method, "Target.attachedToTarget");
    assert_eq!(child.session_id.as_deref(), Some(tab_session.as_str()));
    assert_eq!(child.params["targetInfo"]["type"], "page");
    assert_ne!(child.params["targetInfo"]["targetId"], tab_target);
    assert!(recorder.0.len() <= MAX_RECORDED_PROTOCOL_ITEMS);
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
    let filtered = connection([Capability::SessionRead])
        .await
        .dispatch(CdpRequest::new(
            3,
            "Target.setAutoAttach",
            json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true, "filter": [{"type":"page", "exclude":true}, {"type":"iframe"}]}),
        ))
        .await;
    assert!(filtered.error().is_none(), "{:?}", filtered.error());

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
async fn discovery_and_auto_attach_filters_are_stateful_and_fail_closed() {
    let connection = connection([Capability::SessionRead]).await;
    let disabled = connection
        .dispatch(CdpRequest::new(
            1,
            "Target.setDiscoverTargets",
            json!({"discover": false}),
        ))
        .await;
    assert!(disabled.error().is_none());
    assert!(connection.next_event().await.is_none());

    let excluded = connection
        .dispatch(CdpRequest::new(
            2,
            "Target.setDiscoverTargets",
            json!({
                "discover": true, "filter": [{"type": "page", "exclude": true}]
            }),
        ))
        .await;
    assert!(excluded.error().is_none());
    assert!(
        connection.next_event().await.is_none(),
        "excluded target must not be discovered"
    );

    let matching = connection
        .dispatch(CdpRequest::new(
            3,
            "Target.setDiscoverTargets",
            json!({
                "discover": true, "filter": [{"type": "page"}]
            }),
        ))
        .await;
    assert!(matching.error().is_none());
    assert_eq!(
        connection.next_event().await.unwrap().method,
        "Target.targetCreated"
    );

    let off = connection
        .dispatch(CdpRequest::new(
            4,
            "Target.setAutoAttach",
            json!({
                "autoAttach": false, "waitForDebuggerOnStart": false, "flatten": true,
                "filter": [{"type": "page"}]
            }),
        ))
        .await;
    assert!(off.error().is_none());
    assert!(
        connection.next_event().await.is_none(),
        "auto-attach off must suppress attachment"
    );

    let filtered = connection
        .dispatch(CdpRequest::new(
            5,
            "Target.setAutoAttach",
            json!({
                "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true,
                "filter": [{"type": "page", "exclude": true}]
            }),
        ))
        .await;
    assert!(filtered.error().is_none());
    assert!(
        connection.next_event().await.is_none(),
        "excluded target must not be attached"
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

#[tokio::test]
async fn puppeteer_user_agent_compatibility_is_an_exact_current_value_noop() {
    let allowed = connection([Capability::BrowserMutate])
        .await
        .dispatch(CdpRequest::new(
            91,
            "Network.setUserAgentOverride",
            json!({"userAgent":"AutomationRuntime/0.1"}),
        ))
        .await;
    assert!(allowed.error().is_none());
    let mutation = connection([Capability::BrowserMutate])
        .await
        .dispatch(CdpRequest::new(
            92,
            "Network.setUserAgentOverride",
            json!({"userAgent":"mutated-agent"}),
        ))
        .await;
    assert_eq!(
        mutation.error().unwrap().code,
        CdpErrorCode::InvalidParams as i32
    );
}

#[tokio::test]
async fn runtime_and_network_events_are_suppressed_until_each_domain_is_enabled() {
    let connection = connection([
        Capability::JavascriptEvaluate,
        Capability::PageRead,
        Capability::SessionRead,
    ])
    .await;
    connection
        .queue_event(CdpEvent {
            method: "Runtime.executionContextCreated".into(),
            params: json!({"context":{"uniqueId":"before-enable"}}),
            session_id: None,
        })
        .await
        .unwrap();
    connection
        .queue_event(CdpEvent {
            method: "Network.loadingFailed".into(),
            params: json!({"requestId":"before-enable","errorText":"suppressed","canceled":true}),
            session_id: None,
        })
        .await
        .unwrap();
    assert!(connection.next_event().await.is_none());

    assert!(connection
        .dispatch(CdpRequest::new(1, "Runtime.enable", json!({})))
        .await
        .error()
        .is_none());
    assert_eq!(
        connection.next_event().await.unwrap().method,
        "Runtime.executionContextCreated"
    );
    assert!(connection
        .dispatch(CdpRequest::new(2, "Network.enable", json!({})))
        .await
        .error()
        .is_none());
    connection
        .queue_event(CdpEvent {
            method: "Network.loadingFailed".into(),
            params: json!({"requestId":"enabled","errorText":"observed","canceled":true}),
            session_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        connection.next_event().await.unwrap().method,
        "Network.loadingFailed"
    );
}

#[tokio::test]
async fn lifecycle_events_require_page_and_lifecycle_configuration() {
    let connection = connection([Capability::PageRead]).await;
    let event = || CdpEvent {
        method: "Page.lifecycleEvent".into(),
        params: json!({"frameId":"frame","loaderId":"loader","name":"load","timestamp":0}),
        session_id: None,
    };
    connection.queue_event(event()).await.unwrap();
    assert!(connection.next_event().await.is_none());
    assert!(connection
        .dispatch(CdpRequest::new(1, "Page.enable", json!({})))
        .await
        .error()
        .is_none());
    connection.queue_event(event()).await.unwrap();
    assert!(connection.next_event().await.is_none());
    assert!(connection
        .dispatch(CdpRequest::new(
            2,
            "Page.setLifecycleEventsEnabled",
            json!({"enabled":true})
        ))
        .await
        .error()
        .is_none());
    connection.queue_event(event()).await.unwrap();
    assert_eq!(
        connection.next_event().await.unwrap().method,
        "Page.lifecycleEvent"
    );
}
