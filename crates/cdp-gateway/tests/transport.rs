mod support;

use std::{collections::BTreeSet, future::IntoFuture, sync::Arc};

use cdp_gateway::{CdpGateway, MethodRegistry};
use chrono::{Duration, Utc};
use futures_util::{SinkExt, StreamExt};
use interface_core::AuthorityStore;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue, Message};
use types::{Capability, PrincipalId};

#[tokio::test]
async fn websocket_dispatches_multiple_requests_concurrently_and_preserves_ids() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let authority = Arc::new(AuthorityStore::in_memory());
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [Capability::SessionRead, Capability::FileDownload],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let runtime = support::BlockingRuntime::new();
    let gateway = Arc::new(CdpGateway::new(
        authority,
        runtime.clone(),
        MethodRegistry::compiled(),
        format!("ws://{address}"),
    ));
    let websocket_url = gateway
        .version(Some(&token))
        .await
        .unwrap()
        .web_socket_debugger_url;
    let server = tokio::spawn(axum::serve(listener, gateway.router()).into_future());
    let mut request = websocket_url.into_client_request().unwrap();
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    socket
        .send(Message::Text(
            r#"{"id":41,"method":"Target.getTargets","params":{}}"#.into(),
        ))
        .await
        .unwrap();
    socket
        .send(Message::Text(
            r#"{"id":42,"method":"Target.getTargets","params":{}}"#.into(),
        ))
        .await
        .unwrap();
    runtime.wait_for_active(2).await;
    runtime.release_all();
    let mut ids = BTreeSet::new();
    for _ in 0..2 {
        let message = socket.next().await.unwrap().unwrap().into_text().unwrap();
        ids.insert(
            serde_json::from_str::<serde_json::Value>(&message).unwrap()["id"]
                .as_u64()
                .unwrap(),
        );
    }
    assert_eq!(ids, BTreeSet::from([41, 42]));
    socket.close(None).await.unwrap();
    server.abort();
}

#[tokio::test]
async fn websocket_drains_generation_teardown_events_before_target_disappears() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let now = Utc::now();
    let session = types::SessionId::new();
    let runtime_session = session.0.to_string();
    let runtime = Arc::new(support::StaticRuntime {
        sessions: vec![types::SessionState {
            id: session,
            profile: "p".into(),
            proxy: None,
            page_ids: vec![types::PageId::new()],
            created_at: now,
            last_used_at: now,
            execution_policy: types::ExecutionPolicy::default(),
        }],
    });
    let authority = Arc::new(AuthorityStore::in_memory());
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [Capability::SessionRead, Capability::FileDownload],
            now + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let gateway = Arc::new(CdpGateway::new(
        authority,
        runtime,
        MethodRegistry::compiled(),
        format!("ws://{address}"),
    ));
    let websocket_url = gateway
        .version(Some(&token))
        .await
        .unwrap()
        .web_socket_debugger_url;
    let server = tokio::spawn(axum::serve(listener, gateway.clone().router()).into_future());
    let mut request = websocket_url.into_client_request().unwrap();
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    socket
        .send(Message::Text(
            r#"{"id":1,"method":"Target.getTargets","params":{}}"#.into(),
        ))
        .await
        .unwrap();
    let response = socket.next().await.unwrap().unwrap().into_text().unwrap();
    let target = serde_json::from_str::<serde_json::Value>(&response).unwrap()["result"]
        ["targetInfos"][0]["targetId"]
        .as_str()
        .unwrap()
        .to_owned();
    gateway
        .replace_worker_generation(&runtime_session, cdp_gateway::RuntimeGeneration(2))
        .await
        .unwrap();
    let event = socket.next().await.unwrap().unwrap().into_text().unwrap();
    let event: serde_json::Value = serde_json::from_str(&event).unwrap();
    assert_eq!(event["method"], "Target.targetDestroyed");
    assert_eq!(event["params"]["targetId"], target);
    socket
        .send(Message::Text(
            r#"{"id":2,"method":"Target.getTargets","params":{}}"#.into(),
        ))
        .await
        .unwrap();
    loop {
        let message = socket.next().await.unwrap().unwrap().into_text().unwrap();
        let value: serde_json::Value = serde_json::from_str(&message).unwrap();
        if value["id"] == 2 {
            break;
        }
    }
    gateway
        .replace_worker_generation(&runtime_session, cdp_gateway::RuntimeGeneration(2))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), socket.next())
            .await
            .is_err()
    );
    socket.close(None).await.unwrap();
    server.abort();
}
