use artifact_store::ArtifactStore;
use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};
use interface_core::{
    ArtifactBoundaryTestObserver, ArtifactOwnershipLimits, ArtifactReader, AuthorityStore, Event,
    EventGapReason, EventStore, RuntimeInterface, SessionOwnershipRegistry,
};
use page_runtime::PageRuntime;
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use session_manager::SessionManager;
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Notify};
use types::{
    AttemptId, Capability, ClickCommand, CommandEnvelope, CommandError, CommandId, CommandOutcome,
    CreateSessionRequest, Evidence, InspectCommand, NavigateCommand, OpenPageRequest, PageId,
    PrimitiveCommand, PrincipalId, SessionId, TypeTextCommand, WorkerId, WorkflowId,
};
use worker_pool::{BrowserWorker, WorkerFactory, WorkerPool};
use workflow_journal::JsonlJournal;

struct CapacityFactory {
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    released: Arc<AtomicUsize>,
    wake: Arc<Notify>,
    started: Arc<Mutex<Vec<String>>>,
}
struct SlowArtifactRead;
impl ArtifactBoundaryTestObserver for SlowArtifactRead {
    fn before_artifact_read(&self) {
        std::thread::sleep(Duration::from_millis(100));
    }
}
struct CapacityWorker {
    id: WorkerId,
    profile: PathBuf,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    released: Arc<AtomicUsize>,
    wake: Arc<Notify>,
    started: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl WorkerFactory for CapacityFactory {
    async fn launch(&self, session: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        Ok(Arc::new(CapacityWorker {
            id: WorkerId::new(),
            profile: PathBuf::from(format!("/capacity/{}", session.0)),
            active: self.active.clone(),
            peak: self.peak.clone(),
            released: self.released.clone(),
            wake: self.wake.clone(),
            started: self.started.clone(),
        }))
    }
}
#[async_trait]
impl BrowserWorker for CapacityWorker {
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
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        self.started.lock().await.push(command.url.clone());
        while self.released.load(Ordering::SeqCst) == 0 {
            self.wake.notified().await;
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(vec![Evidence::Navigation {
            url: command.url.clone(),
            title: "capacity".into(),
        }])
    }
    async fn inspect(&self, _: &PageId, _: &InspectCommand) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![])
    }
    async fn click(&self, _: &PageId, _: &ClickCommand) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![])
    }
    async fn type_text(
        &self,
        _: &PageId,
        _: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![])
    }
    async fn close(&self) -> Result<(), CommandError> {
        Ok(())
    }
}

async fn http_request(stream: &mut TcpStream, request: &[u8]) {
    stream.write_all(request).await.unwrap();
    let mut response = Vec::new();
    let mut chunk = [0_u8; 128];
    while !response.windows(4).any(|window| window == b"\r\n\r\n") {
        let count = stream.read(&mut chunk).await.unwrap();
        assert!(count > 0);
        response.extend_from_slice(&chunk[..count]);
    }
    assert!(response.starts_with(b"HTTP/1.1 200"));
}

async fn submit_capacity(
    runtime: RuntimeService,
    session: SessionId,
    page: PageId,
    url: &str,
) -> CommandOutcome {
    runtime
        .submit(CommandEnvelope {
            schema_version: 1,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session,
            page_id: Some(page),
            deadline: Utc::now() + ChronoDuration::seconds(10),
            command: PrimitiveCommand::Navigate(NavigateCommand {
                url: url.into(),
                wait_until: types::WaitUntil::DomContentLoaded,
                timeout_ms: 5_000,
            }),
        })
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn production_listener_enforces_sixty_four_live_connections() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let authority = Arc::new(AuthorityStore::in_memory());
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [Capability::SessionRead],
            Utc::now() + ChronoDuration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let runtime = RuntimeService::default();
    let app = broker::router(broker::AppState::new(
        authority,
        move |handle| {
            Arc::new(AuthenticatedRuntime::new(runtime.clone(), handle))
                as Arc<dyn RuntimeInterface>
        },
        config::InterfaceConfig::default(),
    ));
    let rejection_stats = broker::RejectionWorkerStats::default();
    let server = tokio::spawn(broker::serve_listener_with_rejection_limit(
        listener,
        app,
        64,
        16,
        rejection_stats.clone(),
    ));
    let request=format!("GET /v1/runtime HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nx-interface-version: {}\r\nx-correlation-id: {}\r\nx-deadline: {}\r\n\r\n",types::CURRENT_INTERFACE_VERSION,uuid::Uuid::new_v4(),(Utc::now()+ChronoDuration::seconds(30)).to_rfc3339());
    let mut clients = Vec::new();
    for _ in 0..64 {
        let mut client = TcpStream::connect(address).await.unwrap();
        http_request(&mut client, request.as_bytes()).await;
        clients.push(client);
    }
    let mut overflow = TcpStream::connect(address).await.unwrap();
    overflow.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    overflow.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8(response).unwrap();
    assert!(response.starts_with("HTTP/1.1 429"), "{response}");
    assert!(response.contains("retry-after: 1\r\n"), "{response}");
    assert!(
        response.contains("\"code\":\"resourceExhausted\""),
        "{response}"
    );
    assert!(response.contains("\"retryable\":true"), "{response}");
    assert!(response.contains("\"retryAfterMs\":1000"), "{response}");

    let mut slow_rejections = Vec::new();
    for _ in 0..16 {
        slow_rejections.push(TcpStream::connect(address).await.unwrap());
    }
    tokio::time::timeout(Duration::from_secs(1), async {
        while rejection_stats.active() < 16 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("authenticated listener should saturate only the configured rejectors");
    let mut flood_reads = tokio::task::JoinSet::new();
    for _ in 0..256 {
        let mut peer = TcpStream::connect(address).await.unwrap();
        flood_reads.spawn(async move {
            let mut byte = [0_u8; 1];
            tokio::time::timeout(Duration::from_millis(250), peer.read(&mut byte))
                .await
                .is_ok_and(|read| read.is_ok_and(|count| count == 0))
        });
    }
    while let Some(closed) = flood_reads.join_next().await {
        assert!(
            closed.unwrap(),
            "excess authenticated peer was not promptly dropped"
        );
    }
    for mut peer in slow_rejections {
        let mut overload = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), peer.read_to_end(&mut overload))
            .await
            .expect("bounded rejector should finish")
            .unwrap();
        assert!(overload.starts_with(b"HTTP/1.1 429"));
    }
    tokio::time::timeout(Duration::from_secs(1), async {
        while rejection_stats.active() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("rejector accounting should drain after the authenticated flood");
    assert_eq!(rejection_stats.peak(), 16);

    drop(clients.pop());
    let retry_started = std::time::Instant::now();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut admitted = TcpStream::connect(address).await.unwrap();
    http_request(&mut admitted, request.as_bytes()).await;
    assert!(retry_started.elapsed() >= Duration::from_millis(1_000));
    drop(clients);
    drop(admitted);
    let oversized = reqwest::Client::new()
        .post(format!("http://{address}/v1/sessions"))
        .header("authorization", format!("Bearer {token}"))
        .header("x-interface-version", types::CURRENT_INTERFACE_VERSION)
        .header("x-correlation-id", uuid::Uuid::new_v4().to_string())
        .header(
            "x-deadline",
            (Utc::now() + ChronoDuration::seconds(30)).to_rfc3339(),
        )
        .header("content-type", "application/json")
        .body(vec![
            b'x';
            config::InterfaceConfig::default().max_request_bytes + 1
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(oversized.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_authenticated_artifact_readers_use_the_bounded_production_reader() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(root.path(), 4096, 4096);
    let session = SessionId::new();
    let record = store
        .put(
            &session,
            &PageId::new(),
            "application/octet-stream",
            "bin",
            b"bounded-artifact",
            4096,
        )
        .await
        .unwrap();
    let authority = AuthorityStore::in_memory();
    let principal = PrincipalId::from_uuid(uuid::Uuid::new_v4());
    let token = authority
        .issue(
            principal.clone(),
            [Capability::ArtifactRead, Capability::ArtifactCapture],
            Utc::now() + ChronoDuration::minutes(5),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let context = handle.context(Utc::now() + ChronoDuration::minutes(1), None);
    let (ownership, recorder) = SessionOwnershipRegistry::bounded(32);
    recorder
        .record_authenticated_session(principal, session.clone())
        .unwrap();
    let reader = ArtifactReader::new_with_test_observer(
        store,
        ownership,
        4096,
        ArtifactOwnershipLimits {
            max_records: 32,
            max_bytes: 4096,
        },
        Arc::new(SlowArtifactRead),
    )
    .unwrap();
    let reference = reader
        .register(&handle, &context, &session, &record)
        .await
        .unwrap();

    let mut reads = Vec::new();
    for _ in 0..64 {
        let (reader, handle, context, session, reference) = (
            reader.clone(),
            handle.clone(),
            context.clone(),
            session.clone(),
            reference.clone(),
        );
        reads.push(tokio::spawn(async move {
            reader.read(&handle, &context, &session, &reference).await
        }));
    }
    let mut completed = 0;
    let mut overloaded = 0;
    for read in reads {
        match read.await.unwrap() {
            Ok(content) => {
                assert_eq!(content.bytes, b"bounded-artifact");
                completed += 1;
            }
            Err(error) => {
                assert_eq!(error.code, types::InterfaceErrorCode::ResourceExhausted);
                assert!(error.retryable);
                assert_eq!(error.retry_after_ms, Some(25));
                overloaded += 1;
            }
        }
    }
    assert_eq!(completed, 8);
    assert_eq!(overloaded, 56);
    assert_eq!(reader.peak_concurrent_reads_for_tests(), 8);
    let retry_started = std::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(25)).await;
    let retried = reader
        .read(&handle, &context, &session, &reference)
        .await
        .unwrap();
    assert_eq!(retried.bytes, b"bounded-artifact");
    assert!(retry_started.elapsed() >= Duration::from_millis(25));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thirty_two_real_sessions_run_only_eight_runtime_service_workflows_at_once() {
    let root = tempfile::tempdir().unwrap();
    let peak = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let released = Arc::new(AtomicUsize::new(0));
    let wake = Arc::new(Notify::new());
    let started = Arc::new(Mutex::new(Vec::new()));
    let workers = Arc::new(WorkerPool::new(
        8,
        Arc::new(CapacityFactory {
            active: active.clone(),
            peak: peak.clone(),
            released: released.clone(),
            wake: wake.clone(),
            started: started.clone(),
        }),
    ));
    let runtime = RuntimeService::new(
        SessionManager::new(workers.clone()),
        PageRuntime::new(
            Arc::new(
                JsonlJournal::open(root.path().join("commands.jsonl"))
                    .await
                    .unwrap(),
            ),
            workers,
        ),
    );
    let mut targets = Vec::new();
    for index in 0..32 {
        let session = tokio::time::timeout(
            Duration::from_millis(100),
            runtime.create_session(CreateSessionRequest {
                profile: format!("warm-{index}"),
                proxy: None,
                execution_policy: Default::default(),
            }),
        )
        .await
        .expect("warm session retained active permit")
        .unwrap();
        let page = runtime
            .open_page(OpenPageRequest {
                session_id: session.id.clone(),
            })
            .await
            .unwrap();
        targets.push((session.id, page.id));
    }
    assert_eq!(runtime.list_sessions().await.len(), 32);
    let mut workflows = Vec::new();
    for (session, page) in targets.iter().take(8).cloned() {
        let runtime = runtime.clone();
        workflows.push(tokio::spawn(async move {
            submit_capacity(runtime, session, page, "https://bulk.test/").await
        }));
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while active.load(Ordering::SeqCst) < 8 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (session, page) = targets[8].clone();
    let runtime_clone = runtime.clone();
    workflows.push(tokio::spawn(async move {
        submit_capacity(runtime_clone, session, page, "https://interactive.test/").await
    }));
    tokio::time::sleep(Duration::from_millis(25)).await;
    for (session, page) in targets.iter().skip(9).take(8).cloned() {
        let runtime = runtime.clone();
        workflows.push(tokio::spawn(async move {
            submit_capacity(runtime, session, page, "https://bulk-queued.test/").await
        }));
    }
    assert!(peak.load(Ordering::SeqCst) <= 8);
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert_eq!(
        active.load(Ordering::SeqCst),
        8,
        "ninth workflow escaped active bound"
    );
    released.store(1, Ordering::SeqCst);
    wake.notify_waiters();
    for workflow in workflows {
        assert!(matches!(
            workflow.await.unwrap(),
            CommandOutcome::Completed { .. }
        ));
    }
    assert_eq!(
        started.lock().await[8],
        "https://interactive.test/",
        "FIFO worker admission starved the interactive request"
    );
    assert_eq!(active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn slow_consumers_are_bounded_and_receive_actionable_gap_recovery() {
    let events = EventStore::new(32);
    for index in 0..1024 {
        events
            .append(Event::new("load", serde_json::json!({"index": index})))
            .await;
    }
    let gap = events.read_after(0.into(), 256).await.unwrap_err();
    assert_eq!(gap.reason, EventGapReason::HistoryLost);
    assert_eq!(gap.earliest_available.0, 993);
    let resumed = events.read_after(992.into(), 256).await.unwrap();
    assert_eq!(resumed.events.len(), 32);
}

#[tokio::test]
#[ignore = "requires installed Chromium for warm-session and artifact-reader capacity proof"]
async fn installed_chromium_capacity_fixture_supports_warm_sessions() {
    let harness = interface_conformance::live::ChromeRuntimeHarness::start().await;
    assert_eq!(harness.config.browser.max_active, 8);
    assert_eq!(harness.config.interface.max_connections, 64);
    println!("AUTOMATION_RUNTIME_SECURITY_PROOF:v1:connection-and-workflow-capacity");
}
