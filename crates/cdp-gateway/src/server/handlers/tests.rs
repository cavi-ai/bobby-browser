use super::*;
use crate::server::playwright_semantic_click::{attached_connection, CapturingRuntime};
use std::sync::{Arc, Mutex as StdMutex};

async fn fixture() -> (Arc<CapturingRuntime>, CdpConnection, String) {
    let now = Utc::now();
    let runtime = Arc::new(CapturingRuntime {
        sessions: vec![types::SessionState {
            id: SessionId::new(),
            profile: "fixture".into(),
            proxy: None,
            page_ids: vec![PageId::new()],
            created_at: now,
            last_used_at: now,
            execution_policy: types::ExecutionPolicy::default(),
            zigzagzig: false,
        }],
        submitted: StdMutex::new(vec![]),
    });
    let (connection, session) = attached_connection(runtime.clone()).await;
    (runtime, connection, session)
}

fn context(connection: &CdpConnection) -> RequestContext {
    connection
        .handle
        .context(Utc::now() + Duration::seconds(30), None)
}

fn request(method: &str, session: &str, params: Value) -> CdpRequest {
    let mut request = CdpRequest::new(1, method, params);
    request.session_id = Some(session.into());
    request
}

#[tokio::test]
async fn download_behavior_changes_event_state_only_after_validation() {
    let (_, connection, _) = fixture().await;
    for (params, expected_error, enabled) in [
        (
            json!({"behavior":"allow", "eventsEnabled":true}),
            false,
            true,
        ),
        (
            json!({"behavior":"invalid", "eventsEnabled":false}),
            true,
            true,
        ),
        (
            json!({"behavior":"allow", "eventsEnabled":"yes"}),
            true,
            true,
        ),
        (
            json!({"behavior":"deny", "eventsEnabled":true}),
            false,
            false,
        ),
    ] {
        let response = connection
            .handle_browser_set_download_behavior(
                CdpRequest::new(1, "Browser.setDownloadBehavior", params),
                context(&connection),
            )
            .await;
        assert_eq!(response.error().is_some(), expected_error);
        assert_eq!(*connection.download_events_enabled.lock().await, enabled);
    }
}

#[tokio::test]
async fn focus_handler_refuses_bad_parameters_and_requires_verified_runtime_evidence() {
    let (runtime, connection, session) = fixture().await;
    for params in [
        json!({}),
        json!({"enabled":"true"}),
        json!({"enabled":true,"extra":1}),
    ] {
        let response = connection
            .handle_emulation_set_focus(
                request("Emulation.setFocusEmulationEnabled", &session, params),
                context(&connection),
            )
            .await;
        assert_eq!(
            response.error().unwrap().code,
            CdpErrorCode::InvalidParams as i32
        );
    }
    assert!(runtime.submitted.lock().unwrap().is_empty());
    let response = connection
        .handle_emulation_set_focus(
            request(
                "Emulation.setFocusEmulationEnabled",
                &session,
                json!({"enabled":true}),
            ),
            context(&connection),
        )
        .await;
    assert_eq!(
        response.error().unwrap().code,
        CdpErrorCode::RuntimeFailure as i32
    );
    assert!(
        matches!(&runtime.submitted.lock().unwrap()[0],PrimitiveCommand::SetFocusEmulation(input) if input.enabled)
    );
}

#[tokio::test]
async fn metrics_and_touch_refuse_emulation_the_runtime_cannot_apply() {
    let (runtime, connection, session) = fixture().await;
    for params in [
        json!({"width":0,"height":900}),
        json!({"width":1200,"height":900,"deviceScaleFactor":2}),
        json!({"width":1200,"height":900,"screenOrientation":{"angle":90,"type":"landscapePrimary"}}),
    ] {
        let response = connection
            .handle_emulation_set_device_metrics(
                request("Emulation.setDeviceMetricsOverride", &session, params),
                context(&connection),
            )
            .await;
        assert_eq!(
            response.error().unwrap().code,
            CdpErrorCode::InvalidParams as i32
        );
    }
    assert!(runtime.submitted.lock().unwrap().is_empty());
    for (enabled, succeeds) in [(false, true), (true, false)] {
        let response = connection
            .handle_emulation_set_touch(
                request(
                    "Emulation.setTouchEmulationEnabled",
                    &session,
                    json!({"enabled":enabled}),
                ),
                context(&connection),
            )
            .await;
        assert_eq!(response.error().is_none(), succeeds);
    }
}

#[tokio::test]
async fn frame_tree_uses_session_pending_load_and_refuses_unknown_sessions() {
    let (_, connection, session) = fixture().await;
    // This is state captured by the navigation handler while a load is pending.
    connection.pending_page_loads.lock().await.insert(
        session.clone(),
        (
            "frame-fixture".into(),
            "https://example.test/loading".into(),
            "loader-fixture".into(),
        ),
    );
    let response = connection
        .handle_page_get_frame_tree(
            request("Page.getFrameTree", &session, json!({})),
            context(&connection),
        )
        .await;
    assert!(response.error().is_none());
    let response = serde_json::to_value(response).unwrap();
    assert_eq!(
        response["result"]["frameTree"]["frame"]["url"],
        "https://example.test/loading"
    );
    assert_eq!(
        response["result"]["frameTree"]["frame"]["loaderId"],
        "loader-fixture"
    );
    let refused = connection
        .handle_page_get_frame_tree(
            request("Page.getFrameTree", "unknown", json!({})),
            context(&connection),
        )
        .await;
    assert_eq!(
        refused.error().unwrap().code,
        CdpErrorCode::InvalidParams as i32
    );
}
