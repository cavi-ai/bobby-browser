use artifact_store::ArtifactStore;
use async_trait::async_trait;
use axum::{routing::get, Router};
use chrono::{Duration as ChronoDuration, Utc};
use interface_core::{
    ArtifactOwnershipLimits, ArtifactReader, AuthorityStore, Event, EventGapReason, EventStore,
    SessionOwnershipRegistry,
};
use page_runtime::PageRuntime;
use sdk_core::RuntimeService;
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
use tokio::sync::Notify;
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
}
struct CapacityWorker {
    id: WorkerId,
    profile: PathBuf,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    released: Arc<AtomicUsize>,
    wake: Arc<Notify>,
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

async fn http_request(stream: &mut TcpStream) {
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    let mut chunk = [0_u8; 128];
    while !response.windows(4).any(|window| window == b"\r\n\r\n") {
        let count = stream.read(&mut chunk).await.unwrap();
        assert!(count > 0);
        response.extend_from_slice(&chunk[..count]);
    }
    assert!(response.starts_with(b"HTTP/1.1 200"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn production_listener_enforces_sixty_four_live_connections() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(broker::serve_listener(
        listener,
        Router::new().route("/healthz", get(|| async { "ok" })),
        64,
    ));
    let mut clients = Vec::new();
    for _ in 0..64 {
        let mut client = TcpStream::connect(address).await.unwrap();
        http_request(&mut client).await;
        clients.push(client);
    }
    let mut overflow = TcpStream::connect(address).await.unwrap();
    overflow
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut byte = [0_u8; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(100), overflow.read(&mut byte))
            .await
            .is_err()
    );
    drop(clients.pop());
    let count = tokio::time::timeout(Duration::from_secs(2), overflow.read(&mut byte))
        .await
        .expect("overflow connection was not admitted after release")
        .unwrap();
    assert_eq!(count, 1);
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
    let reader = ArtifactReader::new(
        store,
        ownership,
        4096,
        ArtifactOwnershipLimits {
            max_records: 32,
            max_bytes: 4096,
        },
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
            reader
                .read(&handle, &context, &session, &reference)
                .await
                .unwrap()
                .bytes
        }));
    }
    for read in reads {
        assert_eq!(read.await.unwrap(), b"bounded-artifact");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thirty_two_real_sessions_run_only_eight_runtime_service_workflows_at_once() {
    let root = tempfile::tempdir().unwrap();
    let peak = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let released = Arc::new(AtomicUsize::new(0));
    let wake = Arc::new(Notify::new());
    let workers = Arc::new(WorkerPool::new(
        8,
        Arc::new(CapacityFactory {
            active: active.clone(),
            peak: peak.clone(),
            released: released.clone(),
            wake: wake.clone(),
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
    for (session, page) in targets.iter().take(9).cloned() {
        let runtime = runtime.clone();
        workflows.push(tokio::spawn(async move {
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
                        url: "https://capacity.test/".into(),
                        wait_until: types::WaitUntil::DomContentLoaded,
                        timeout_ms: 5_000,
                    }),
                })
                .await
        }));
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while active.load(Ordering::SeqCst) < 8 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
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
}
