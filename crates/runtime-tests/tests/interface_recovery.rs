use std::{
    future::IntoFuture,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration as StdDuration,
};

use async_trait::async_trait;
use chrono::{Duration, Utc};
use futures_util::{SinkExt, StreamExt};
use interface_core::{AuthorityStore, Event, EventGapReason, EventStore, RuntimeInterface};
use page_runtime::{ExecutionPhaseObserver, PageRuntime};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use session_manager::SessionManager;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue, Message};
use types::{
    AttemptId, Capability, ClickCommand, CommandEnvelope, CommandError, CommandId, CommandOutcome,
    CommandPhase, CreateSessionRequest, DownloadUrlCommand, Evidence, InspectCommand,
    NavigateCommand, OpenPageRequest, PageId, PrimitiveCommand, PrincipalId, RuntimeCommand,
    SessionId, TypeTextCommand, WorkerId, WorkflowId,
};
use worker_pool::{BrowserWorker, WorkerFactory, WorkerPool};
use workflow_journal::JsonlJournal;

struct PauseAt {
    target: CommandPhase,
    reached: Notify,
    release: Notify,
}

struct ProcessPause {
    marker: PathBuf,
}
#[async_trait]
impl ExecutionPhaseObserver for ProcessPause {
    async fn durable_phase_reached(&self, phase: CommandPhase) {
        if phase == CommandPhase::Executing {
            let temporary = self.marker.with_extension("tmp");
            std::fs::write(&temporary, b"executing").unwrap();
            std::fs::rename(temporary, &self.marker).unwrap();
            std::future::pending::<()>().await;
        }
    }
}

#[async_trait]
impl ExecutionPhaseObserver for PauseAt {
    async fn durable_phase_reached(&self, phase: CommandPhase) {
        if phase == self.target {
            self.reached.notify_one();
            self.release.notified().await;
        }
    }
}

struct TestFactory;
struct TestWorker {
    id: WorkerId,
    profile: PathBuf,
}

#[async_trait]
impl WorkerFactory for TestFactory {
    async fn launch(&self, session: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        Ok(Arc::new(TestWorker {
            id: WorkerId::new(),
            profile: PathBuf::from(format!("/profiles/{}", session.0)),
        }))
    }
}

#[async_trait]
impl BrowserWorker for TestWorker {
    fn worker_id(&self) -> WorkerId {
        self.id.clone()
    }
    fn profile_dir(&self) -> &Path {
        &self.profile
    }
    async fn open_page(&self, _: PageId) -> Result<(), CommandError> {
        Ok(())
    }
    async fn navigate(
        &self,
        _: &PageId,
        command: &NavigateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![Evidence::Navigation {
            url: command.url.clone(),
            title: "fixture".into(),
        }])
    }
    async fn inspect(
        &self,
        _: &PageId,
        command: &InspectCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![Evidence::Inspection {
            selector: command.selector.clone(),
            url: "https://fixture.test/".into(),
            title: "fixture".into(),
            text: "fixture".into(),
            html: None,
        }])
    }
    async fn click(
        &self,
        _: &PageId,
        command: &ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![Evidence::Element {
            selector: command.selector.clone(),
            text: None,
        }])
    }
    async fn type_text(
        &self,
        _: &PageId,
        command: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![Evidence::Element {
            selector: command.selector.clone(),
            text: Some(command.value.clone()),
        }])
    }
    async fn http_state(
        &self,
        _: &PageId,
    ) -> Result<network_engine::state::HttpStateSnapshot, CommandError> {
        Ok(network_engine::state::HttpStateSnapshot {
            version: 1,
            current_url: "about:blank".into(),
            cookies: Vec::new(),
            cache_validators: Default::default(),
            user_agent: "release-recovery".into(),
            language: "en".into(),
        })
    }
    async fn commit_http_state(
        &self,
        _: &PageId,
        _: u64,
        _: network_engine::state::ResponseStateDelta,
    ) -> Result<(), CommandError> {
        Ok(())
    }
    async fn close(&self) -> Result<(), CommandError> {
        Ok(())
    }
}

async fn service(path: &Path, observer: Option<Arc<dyn ExecutionPhaseObserver>>) -> RuntimeService {
    let journal = Arc::new(JsonlJournal::open(path).await.unwrap());
    let workers = Arc::new(WorkerPool::new(8, Arc::new(TestFactory)));
    let mut pages = PageRuntime::new(journal, workers.clone());
    if let Some(observer) = observer {
        pages = pages.with_execution_phase_observer(observer);
    }
    RuntimeService::new(SessionManager::new(workers), pages)
}

async fn adaptive_service(
    path: &Path,
    artifacts: &Path,
    observer: Option<Arc<dyn ExecutionPhaseObserver>>,
) -> RuntimeService {
    let journal = Arc::new(JsonlJournal::open(path).await.unwrap());
    let workers = Arc::new(WorkerPool::new(8, Arc::new(TestFactory)));
    let network = network_engine::NetworkPolicy {
        allow_loopback: true,
        ..Default::default()
    };
    let adaptive = page_runtime::AdaptivePageEngine::new(
        network_engine::EligibilityPolicy::new(network.clone()),
        network_engine::DirectHttpExecutor::new(network.clone()),
        artifact_store::ArtifactStore::new(artifacts, network.max_download_bytes, 16_384),
        network,
    );
    let mut pages = PageRuntime::new_adaptive(journal, workers.clone(), None, adaptive);
    if let Some(observer) = observer {
        pages = pages.with_execution_phase_observer(observer);
    }
    RuntimeService::new(SessionManager::new(workers), pages)
}

async fn download_fixture() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 2048];
        let _ = socket.read(&mut request).await.unwrap();
        let body = b"durable-result-prepared";
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.write_all(body).await.unwrap();
    });
    format!("http://{address}/artifact.bin")
}

fn interface_headers(token: &str) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
    headers.insert(
        "x-interface-version",
        types::CURRENT_INTERFACE_VERSION.parse().unwrap(),
    );
    headers.insert(
        "x-correlation-id",
        uuid::Uuid::new_v4().to_string().parse().unwrap(),
    );
    headers.insert(
        "x-deadline",
        (Utc::now() + Duration::seconds(30))
            .to_rfc3339()
            .parse()
            .unwrap(),
    );
    headers
}

#[tokio::test]
async fn broker_disconnect_after_durable_executing_rebuilds_without_guessing() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("broker-loss.jsonl");
    let observer = Arc::new(PauseAt {
        target: CommandPhase::Executing,
        reached: Notify::new(),
        release: Notify::new(),
    });
    let runtime = service(&path, Some(observer.clone())).await;
    let authority = Arc::new(AuthorityStore::in_memory());
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [
                Capability::SessionRead,
                Capability::SessionWrite,
                Capability::PageRead,
                Capability::PageWrite,
                Capability::BrowserMutate,
            ],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let bound = runtime.clone();
    let app = broker::router(broker::AppState::new(
        authority,
        move |handle| {
            Arc::new(AuthenticatedRuntime::new(bound.clone(), handle)) as Arc<dyn RuntimeInterface>
        },
        config::InterfaceConfig::default(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(axum::serve(listener, app).into_future());
    let client = reqwest::Client::new();
    let endpoint = format!("http://{address}");
    let session_response = client
        .post(format!("{endpoint}/v1/sessions"))
        .headers(interface_headers(&token))
        .header("content-type", "application/json")
        .body(
            serde_json::to_vec(&CreateSessionRequest {
                profile: "broker-loss".into(),
                proxy: None,
                execution_policy: Default::default(),
            })
            .unwrap(),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let session: types::SessionState =
        serde_json::from_slice(&session_response.bytes().await.unwrap()).unwrap();
    let page_response = client
        .post(format!("{endpoint}/v1/pages"))
        .headers(interface_headers(&token))
        .header("content-type", "application/json")
        .body(
            serde_json::to_vec(&OpenPageRequest {
                session_id: session.id.clone(),
            })
            .unwrap(),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let page: types::PageState =
        serde_json::from_slice(&page_response.bytes().await.unwrap()).unwrap();
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: session.id,
        page_id: Some(page.id),
        deadline: Utc::now() + Duration::seconds(20),
        command: RuntimeCommand::Primitive(PrimitiveCommand::Click(ClickCommand {
            selector: "#mutate".into(),
            target: None,
            boundary: false,
            expected_url: None,
        })),
    };
    let command_id = envelope.command_id.clone();
    let request = tokio::spawn({
        let client = client.clone();
        let token = token.clone();
        let endpoint = endpoint.clone();
        async move {
            client
                .post(format!("{endpoint}/v1/commands"))
                .headers(interface_headers(&token))
                .header("content-type", "application/json")
                .body(serde_json::to_vec(&envelope).unwrap())
                .send()
                .await
        }
    });
    tokio::time::timeout(StdDuration::from_secs(2), observer.reached.notified())
        .await
        .expect("broker command did not reach durable executing");
    request.abort();
    let _ = request.await;
    server.abort();
    let _ = server.await;
    observer.release.notify_waiters();
    drop(runtime);
    let rebuilt = service(&path, None).await;
    assert!(matches!(
        rebuilt.pages.recover_command(command_id).await,
        CommandOutcome::NeedsReconciliation { .. }
    ));
}

#[tokio::test]
#[ignore = "spawned as an actual MCP stdio child by the recovery gate"]
async fn mcp_stdio_fixture_process() {
    let Ok(root) = std::env::var("CONFORMANCE_MCP_LOSS_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let marker = root.join("executing.marker");
    let runtime = service(
        &root.join("commands.jsonl"),
        Some(Arc::new(ProcessPause { marker })),
    )
    .await;
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [
                Capability::SessionRead,
                Capability::SessionWrite,
                Capability::PageRead,
                Capability::PageWrite,
                Capability::BrowserMutate,
                Capability::RecoveryRead,
            ],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let server = mcp_gateway::Server::new(Arc::new(AuthenticatedRuntime::new(runtime, handle)));
    server
        .serve(tokio::io::stdin(), tokio::io::stdout())
        .await
        .unwrap();
}

async fn mcp_response(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    id: u64,
) -> serde_json::Value {
    loop {
        let line = lines
            .next_line()
            .await
            .unwrap()
            .expect("MCP child stdout closed");
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
            if value["id"] == id {
                return value;
            }
        }
    }
}
async fn mcp_send(stdin: &mut tokio::process::ChildStdin, value: serde_json::Value) {
    stdin
        .write_all(serde_json::to_string(&value).unwrap().as_bytes())
        .await
        .unwrap();
    stdin.write_all(b"\n").await.unwrap();
}

#[tokio::test]
async fn mcp_stdio_process_termination_after_durable_executing_rebuilds_exactly() {
    let root = tempfile::tempdir().unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "mcp_stdio_fixture_process",
            "--nocapture",
        ])
        .env("CONFORMANCE_MCP_LOSS_ROOT", root.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    mcp_send(&mut stdin,serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"loss-test","version":"1"}}})).await;
    assert!(mcp_response(&mut lines, 1).await.get("result").is_some());
    mcp_send(
        &mut stdin,
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
    )
    .await;
    mcp_send(&mut stdin,serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"session_create","arguments":{"profile":"mcp-loss","proxy":null}}})).await;
    let session = mcp_response(&mut lines, 2).await["result"]["structuredContent"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    mcp_send(&mut stdin,serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"page_open","arguments":{"sessionId":session}}})).await;
    let page = mcp_response(&mut lines, 3).await["result"]["structuredContent"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let command_id = CommandId::new();
    let workflow = WorkflowId::new();
    let envelope = serde_json::json!({"schemaVersion":2,"commandId":command_id,"workflowId":workflow,"attemptId":AttemptId::new(),"sessionId":session,"pageId":page,"deadline":(Utc::now()+Duration::seconds(20)),"command":{"kind":"primitive","input":{"kind":"click","input":{"selector":"#mutate","target":null,"boundary":false,"expectedUrl":null}}}});
    mcp_send(&mut stdin,serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"command_execute","arguments":{"envelope":envelope}}})).await;
    let marker = root.path().join("executing.marker");
    tokio::time::timeout(StdDuration::from_secs(3), async {
        while !marker.exists() {
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
    })
    .await
    .expect("MCP child did not reach durable executing");
    child.kill().await.unwrap();
    let status = child.wait().await.unwrap();
    assert!(!status.success(), "MCP child was not terminated");
    let rebuilt = service(&root.path().join("commands.jsonl"), None).await;
    assert!(matches!(
        rebuilt.pages.recover_command(command_id).await,
        CommandOutcome::NeedsReconciliation { .. }
    ));
}

#[tokio::test]
async fn cdp_websocket_loss_after_durable_executing_rebuilds_exactly() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("cdp-loss.jsonl");
    let observer = Arc::new(PauseAt {
        target: CommandPhase::Executing,
        reached: Notify::new(),
        release: Notify::new(),
    });
    let runtime = service(&path, Some(observer.clone())).await;
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "cdp-loss".into(),
            proxy: None,
            execution_policy: Default::default(),
        })
        .await
        .unwrap();
    runtime
        .open_page(OpenPageRequest {
            session_id: session.id,
        })
        .await
        .unwrap();
    let authority = Arc::new(AuthorityStore::in_memory());
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [
                Capability::SessionRead,
                Capability::PageWrite,
                Capability::BrowserMutate,
            ],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let handle = authority.verify(&token).await.unwrap();
    let authenticated = Arc::new(AuthenticatedRuntime::new(runtime.clone(), handle));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let gateway = Arc::new(cdp_gateway::CdpGateway::new(
        authority,
        authenticated,
        cdp_gateway::MethodRegistry::compiled(),
        format!("ws://{address}"),
    ));
    assert_eq!(gateway.list(Some(&token)).await.unwrap().len(), 1);
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
            serde_json::json!({"id":1,"method":"Target.getTargets","params":{}})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let targets = serde_json::from_str::<serde_json::Value>(
        &socket.next().await.unwrap().unwrap().into_text().unwrap(),
    )
    .unwrap();
    assert!(targets["result"]["targetInfos"][0]["targetId"]
        .as_str()
        .is_some());
    socket
        .send(Message::Text(
            serde_json::json!({"id":2,"method":"Target.setAutoAttach","params":{"autoAttach":true,"waitForDebuggerOnStart":false,"flatten":true,"filter":[{"type":"page"}]}})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let mut attached = None;
    let mut acknowledged = false;
    while attached.is_none() || !acknowledged {
        let value = serde_json::from_str::<serde_json::Value>(
            &socket.next().await.unwrap().unwrap().into_text().unwrap(),
        )
        .unwrap();
        if value["id"] == 2 {
            acknowledged = true;
        }
        if value["method"] == "Target.attachedToTarget" {
            attached = value["params"]["sessionId"].as_str().map(str::to_owned);
        }
    }
    let cdp_session = attached.unwrap();
    socket.send(Message::Text(serde_json::json!({"id":3,"sessionId":cdp_session,"method":"Page.navigate","params":{"url":"https://cdp-loss.test/"}}).to_string().into())).await.unwrap();
    if let Ok(Some(response)) =
        tokio::time::timeout(StdDuration::from_millis(200), socket.next()).await
    {
        panic!(
            "CDP request failed before durable executing: {:?}",
            response.unwrap()
        );
    }
    tokio::time::timeout(StdDuration::from_secs(2), observer.reached.notified())
        .await
        .expect("CDP request did not reach durable executing");
    socket.close(None).await.unwrap();
    drop(socket);
    server.abort();
    let _ = server.await;
    observer.release.notify_waiters();
    drop(runtime);
    let lines = std::fs::read_to_string(&path).unwrap();
    let command_id = lines
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|record| record["phase"] == "executing")
        .expect("durable executing record")["commandId"]
        .as_str()
        .unwrap()
        .parse::<uuid::Uuid>()
        .unwrap();
    let rebuilt = service(&path, None).await;
    assert!(matches!(
        rebuilt.pages.recover_command(CommandId(command_id)).await,
        CommandOutcome::RetryableFailure { .. }
    ));
}

#[tokio::test]
async fn daemon_rebuild_uses_durable_phases_and_never_guesses_after_browser_dispatch() {
    for (phase, must_reconcile) in [
        (CommandPhase::Accepted, false),
        (CommandPhase::Prepared, false),
        (CommandPhase::Executing, true),
        (CommandPhase::Verifying, true),
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("commands.jsonl");
        let observer = Arc::new(PauseAt {
            target: phase,
            reached: Notify::new(),
            release: Notify::new(),
        });
        let runtime = service(&path, Some(observer.clone())).await;
        let session = runtime
            .create_session(CreateSessionRequest {
                profile: "release-recovery".into(),
                proxy: None,
                execution_policy: Default::default(),
            })
            .await
            .unwrap();
        let page = runtime
            .open_page(OpenPageRequest {
                session_id: session.id.clone(),
            })
            .await
            .unwrap();
        let envelope = CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session.id,
            page_id: Some(page.id),
            deadline: Utc::now() + Duration::seconds(10),
            command: RuntimeCommand::Primitive(PrimitiveCommand::Click(ClickCommand {
                selector: "#mutate".into(),
                target: None,
                boundary: false,
                expected_url: None,
            })),
        };
        let command_id = envelope.command_id.clone();
        let task = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.submit(envelope).await }
        });
        tokio::time::timeout(StdDuration::from_secs(2), observer.reached.notified())
            .await
            .expect("durable phase not reached");
        task.abort();
        let _ = task.await;
        observer.release.notify_waiters();
        drop(runtime);

        let rebuilt = service(&path, None).await;
        let outcome = rebuilt.pages.recover_command(command_id).await;
        assert_eq!(
            matches!(outcome, CommandOutcome::NeedsReconciliation { .. }),
            must_reconcile,
            "phase {phase:?}"
        );
        assert_eq!(
            matches!(outcome, CommandOutcome::RetryableFailure { .. }),
            !must_reconcile,
            "phase {phase:?}"
        );
    }
}

#[tokio::test]
async fn result_prepared_abort_rebuilds_from_durable_artifact_state() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("commands.jsonl");
    let artifacts = root.path().join("artifacts");
    let observer = Arc::new(PauseAt {
        target: CommandPhase::ResultPrepared,
        reached: Notify::new(),
        release: Notify::new(),
    });
    let runtime = adaptive_service(&path, &artifacts, Some(observer.clone())).await;
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "result-prepared".into(),
            proxy: None,
            execution_policy: Default::default(),
        })
        .await
        .unwrap();
    let page = runtime
        .open_page(OpenPageRequest {
            session_id: session.id.clone(),
        })
        .await
        .unwrap();
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: session.id,
        page_id: Some(page.id),
        deadline: Utc::now() + Duration::seconds(10),
        command: RuntimeCommand::Primitive(PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
            url: download_fixture().await,
            expected_content_type: Some("application/octet-stream".into()),
            max_bytes: 1024,
        })),
    };
    let command_id = envelope.command_id.clone();
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move { runtime.submit(envelope).await }
    });
    tokio::time::timeout(StdDuration::from_secs(3), observer.reached.notified())
        .await
        .expect("ResultPrepared not durable");
    task.abort();
    let _ = task.await;
    observer.release.notify_waiters();
    drop(runtime);
    tokio::time::sleep(StdDuration::from_millis(50)).await;

    let rebuilt = adaptive_service(&path, &artifacts, None).await;
    assert!(matches!(
        rebuilt.pages.recover_command(command_id).await,
        CommandOutcome::NeedsReconciliation { .. }
    ));
}

#[tokio::test]
async fn worker_generation_replacement_mid_command_rebuilds_without_guessing() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("worker-replacement.jsonl");
    let journal = Arc::new(JsonlJournal::open(&path).await.unwrap());
    let workers = Arc::new(WorkerPool::new(2, Arc::new(TestFactory)));
    let observer = Arc::new(PauseAt {
        target: CommandPhase::Verifying,
        reached: Notify::new(),
        release: Notify::new(),
    });
    let pages =
        PageRuntime::new(journal, workers.clone()).with_execution_phase_observer(observer.clone());
    let runtime = RuntimeService::new(SessionManager::new(workers.clone()), pages);
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "replace".into(),
            proxy: None,
            execution_policy: Default::default(),
        })
        .await
        .unwrap();
    let page = runtime
        .open_page(OpenPageRequest {
            session_id: session.id.clone(),
        })
        .await
        .unwrap();
    let first_worker = workers.lease(session.id.clone()).await.unwrap().worker_id();
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: session.id.clone(),
        page_id: Some(page.id),
        deadline: Utc::now() + Duration::seconds(10),
        command: RuntimeCommand::Primitive(PrimitiveCommand::Click(ClickCommand {
            selector: "#mutate".into(),
            target: None,
            boundary: false,
            expected_url: None,
        })),
    };
    let command_id = envelope.command_id.clone();
    let command = tokio::spawn({
        let runtime = runtime.clone();
        async move { runtime.submit(envelope).await }
    });
    tokio::time::timeout(StdDuration::from_secs(2), observer.reached.notified())
        .await
        .expect("command did not reach durable verifying");
    command.abort();
    let _ = command.await;
    observer.release.notify_waiters();
    workers.invalidate_session(&session.id).await.unwrap();
    let replacement = workers.lease(session.id).await.unwrap().worker_id();
    assert_ne!(first_worker, replacement);

    let rebuilt = service(&path, None).await;
    assert!(matches!(
        rebuilt.pages.recover_command(command_id).await,
        CommandOutcome::NeedsReconciliation { .. }
    ));
}

#[tokio::test]
async fn reconnect_resumes_exactly_or_reports_a_deterministic_gap() {
    let events = EventStore::new(3);
    for sequence in 1..=5 {
        events
            .append(Event::new(
                "boundary",
                serde_json::json!({"sequence": sequence}),
            ))
            .await;
    }
    let exact = events.read_after(3.into(), 8).await.unwrap();
    assert_eq!(
        exact
            .events
            .iter()
            .map(|event| event.cursor.0)
            .collect::<Vec<_>>(),
        [4, 5]
    );
    let gap = events.read_after(1.into(), 8).await.unwrap_err();
    assert_eq!(gap.reason, EventGapReason::HistoryLost);
    assert_eq!(gap.earliest_available.0, 3);
    assert_eq!(
        events.read_after(99.into(), 8).await.unwrap_err().reason,
        EventGapReason::InvalidCursor
    );
}

#[tokio::test]
#[ignore = "requires installed Chromium; exercises daemon/worker replacement fixture"]
async fn installed_chromium_daemon_abort_rebuilds_from_the_same_durable_journal() {
    let harness = interface_conformance::live::ChromeRuntimeHarness::start().await;
    let observer = Arc::new(PauseAt {
        target: CommandPhase::Verifying,
        reached: Notify::new(),
        release: Notify::new(),
    });
    let runtime =
        RuntimeService::build_with_execution_phase_observer(&harness.config, observer.clone())
            .await
            .unwrap();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "chrome-daemon-recovery".into(),
            proxy: None,
            execution_policy: Default::default(),
        })
        .await
        .unwrap();
    let page = runtime
        .open_page(OpenPageRequest {
            session_id: session.id.clone(),
        })
        .await
        .unwrap();
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: session.id,
        page_id: Some(page.id),
        deadline: Utc::now() + Duration::seconds(20),
        command: RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
            url: harness.site_url(),
            wait_until: types::WaitUntil::DomContentLoaded,
            timeout_ms: 15_000,
        })),
    };
    let command_id = envelope.command_id.clone();
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move { runtime.submit(envelope).await }
    });
    tokio::time::timeout(StdDuration::from_secs(20), observer.reached.notified())
        .await
        .expect("real Chrome command did not reach durable verifying");
    task.abort();
    let _ = task.await;
    observer.release.notify_waiters();
    drop(runtime);

    let rebuilt = RuntimeService::build(&harness.config).await.unwrap();
    assert!(matches!(
        rebuilt.pages.recover_command(command_id).await,
        CommandOutcome::RetryableFailure { .. }
    ));
}
