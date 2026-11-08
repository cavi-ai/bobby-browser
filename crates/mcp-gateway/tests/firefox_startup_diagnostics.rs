use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use chrono::{Duration, Utc};
use firefox_companion::selection::{
    build_enrolled_browser_selection, compose_worker_factory_with_pairing_observer,
};
use futures_util::{SinkExt, StreamExt};
use interface_core::AuthorityStore;
use mcp_gateway::Server;
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;
use types::{Capability, CommandError, PrincipalId, ProfileId, SessionId};
use worker_pool::{BrowserWorker, WorkerFactory};

struct CountingFactory {
    inner: Arc<dyn WorkerFactory>,
    launches: AtomicUsize,
}

#[async_trait]
impl WorkerFactory for CountingFactory {
    async fn launch(&self, session: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        self.inner.launch(session).await
    }

    async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}

async fn call(server: &Server, id: u64, method: &str, params: Value) -> Value {
    server
        .handle_message(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
        .await
        .unwrap()
}

#[tokio::test]
async fn workflow_start_reports_the_firefox_attachment_refusal_without_retrying() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    std::fs::create_dir(&profile).unwrap();
    let endpoint_file = profile.join("WebDriverBiDiServer.json");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    std::fs::write(
        &endpoint_file,
        json!({"ws_host":"127.0.0.1","ws_port":listener.local_addr().unwrap().port()}).to_string(),
    )
    .unwrap();
    let probe = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let request = socket.next().await.unwrap().unwrap();
        let request: Value = serde_json::from_str(request.to_text().unwrap()).unwrap();
        assert_eq!(request["method"], "session.status");
        // Remove only this fixture's live endpoint before replying. If repair
        // regresses, the portless enrollment still prevents native launch.
        std::fs::remove_file(endpoint_file).unwrap();
        socket
            .send(Message::Text(
                json!({"type":"success","id":request["id"],
                       "result":{"ready":false,"message":"occupied"}})
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
    });

    let mut config = config::AppConfig::default();
    config.storage.journal_path = root.path().join("commands.jsonl");
    config.storage.checkpoints_dir = root.path().join("checkpoints");
    config.browser.artifacts_dir = root.path().join("artifacts");
    let mut selection = build_enrolled_browser_selection(
        &ProfileId::new(),
        "ws://127.0.0.1/session",
        &profile,
        "127.0.0.1:0".parse().unwrap(),
        &root.path().join("descriptor.json"),
    );
    selection.firefox[0].attachment_only = true;
    selection.firefox[0].timeout_ms = 500;
    let factory = Arc::new(CountingFactory {
        inner: compose_worker_factory_with_pairing_observer(&config, selection, Arc::new(|_| {}))
            .unwrap(),
        launches: AtomicUsize::new(0),
    });
    let runtime = RuntimeService::build_with_worker_factory(&config, factory.clone())
        .await
        .unwrap();
    let authority = AuthorityStore::default();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            vec![
                Capability::SessionRead,
                Capability::SessionWrite,
                Capability::PageWrite,
                Capability::BrowserMutate,
            ],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let server = Server::new(Arc::new(AuthenticatedRuntime::new(runtime, handle)));
    let initialized = call(
        &server,
        1,
        "initialize",
        json!({"protocolVersion":"2025-11-25","capabilities":{},
               "clientInfo":{"name":"test","version":"1"}}),
    )
    .await;
    assert!(initialized.get("result").is_some(), "{initialized}");
    server
        .handle_message(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .await;

    let response = call(
        &server,
        2,
        "tools/call",
        json!({"name":"workflow_start","arguments":{
            "profile":"default","url":"https://example.test/forum","includePostState":true
        }}),
    )
    .await;
    probe.await.unwrap();
    let error = &response["error"];
    assert_eq!(
        error["data"]["interfaceError"]["code"], "engineUnreachable",
        "{response}"
    );
    assert_eq!(error["data"]["interfaceError"]["retryable"], false);
    assert_eq!(
        error["data"]["interfaceError"]["reconciliationRequired"],
        false
    );
    assert!(error["message"].as_str().unwrap().contains(
        "Firefox attachment-only mode refuses an occupied external BiDi session; no browser repair was attempted"
    ), "{response}");
    assert_eq!(factory.launches.load(Ordering::SeqCst), 1);
    let sessions = call(
        &server,
        3,
        "tools/call",
        json!({"name":"session_list","arguments":{}}),
    )
    .await;
    assert_eq!(
        sessions["result"]["structuredContent"]["sessions"],
        json!([]),
        "{sessions}"
    );
    drop(server);
    factory.shutdown().await;
}
