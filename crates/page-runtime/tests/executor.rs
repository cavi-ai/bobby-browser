use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use checkpoint_store::CheckpointStore;
use chrono::{Duration, Utc};
use tokio::sync::Mutex;
use types::{
    AttemptId, CheckpointId, ClickCommand, CommandClass, CommandEnvelope, CommandError, CommandId,
    CommandOutcome, CommandPhase, DownloadUrlCommand, ErrorCode, ErrorLayer, Evidence,
    ExecutionPath, InspectCommand, NavigateCommand, PageId, PrimitiveCommand, SessionId,
    TargetSpec, TypeTextCommand, WaitUntil, WorkerId, WorkflowCheckpoint, WorkflowId,
};
use worker_pool::{BrowserWorker, WorkerFactory, WorkerPool};
use workflow_journal::{CommandJournal, JournalError, JournalRecord, JournalScan};

#[derive(Clone, Copy)]
enum DriverMode {
    Succeed,
    FailInspect,
    FailClick,
    StateConflict,
    CommitFail,
    CommitPause,
}

struct RecordingJournal {
    events: Arc<Mutex<Vec<String>>>,
    fail_on: Option<CommandPhase>,
}

#[async_trait]
impl CommandJournal for RecordingJournal {
    async fn append(&self, record: JournalRecord) -> Result<(), JournalError> {
        if self.fail_on == Some(record.phase) {
            return Err(std::io::Error::other("injected journal failure").into());
        }
        self.events
            .lock()
            .await
            .push(format!("journal:{:?}", record.phase).to_lowercase());
        Ok(())
    }

    async fn history(&self, _: CommandId) -> Result<JournalScan, JournalError> {
        Ok(JournalScan::default())
    }
}

struct FakeWorker {
    id: WorkerId,
    profile: PathBuf,
    events: Arc<Mutex<Vec<String>>>,
    mode: DriverMode,
}

#[async_trait]
impl BrowserWorker for FakeWorker {
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
        self.events.lock().await.push("browser:navigate".into());
        Ok(vec![Evidence::Navigation {
            url: command.url.clone(),
            title: "Fixture".into(),
        }])
    }
    async fn inspect(
        &self,
        _: &PageId,
        command: &InspectCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.events.lock().await.push("browser:inspect".into());
        if matches!(self.mode, DriverMode::FailInspect) {
            return Err(driver_failure());
        }
        Ok(vec![Evidence::Inspection {
            selector: command.selector.clone(),
            url: "https://example.test/".into(),
            title: "Fixture".into(),
            text: command.selector.as_deref().map_or("page", |_| "Ada").into(),
            html: None,
        }])
    }
    async fn click(
        &self,
        _: &PageId,
        command: &ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.events.lock().await.push("browser:click".into());
        if matches!(self.mode, DriverMode::FailClick) {
            return Err(driver_failure());
        }
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
        self.events.lock().await.push("browser:type_text".into());
        Ok(vec![Evidence::Element {
            selector: command.selector.clone(),
            text: Some(command.value.clone()),
        }])
    }
    async fn close(&self) -> Result<(), CommandError> {
        Ok(())
    }
    async fn http_state(
        &self,
        _: &PageId,
    ) -> Result<network_engine::state::HttpStateSnapshot, CommandError> {
        self.events.lock().await.push("http:state".into());
        Ok(network_engine::state::HttpStateSnapshot {
            version: 7,
            current_url: self
                .events
                .lock()
                .await
                .iter()
                .find_map(|event| event.strip_prefix("url:").map(str::to_owned))
                .unwrap_or_else(|| "https://example.test/".into()),
            cookies: Vec::new(),
            cache_validators: Default::default(),
            user_agent: "test".into(),
            language: "en".into(),
        })
    }
    async fn commit_http_state(
        &self,
        _: &PageId,
        _: u64,
        _: network_engine::state::ResponseStateDelta,
    ) -> Result<(), CommandError> {
        self.events.lock().await.push("http:commit".into());
        if matches!(self.mode, DriverMode::CommitPause) {
            std::future::pending::<()>().await;
        }
        if matches!(self.mode, DriverMode::CommitFail) {
            return Err(CommandError {
                code: ErrorCode::BrowserCommandFailed,
                message: "injected state commit failure".into(),
                layer: ErrorLayer::Driver,
                retryable: false,
            });
        }
        if matches!(self.mode, DriverMode::StateConflict) {
            Err(CommandError {
                code: ErrorCode::HttpStateConflict,
                message: "injected conflict".into(),
                layer: ErrorLayer::Driver,
                retryable: true,
            })
        } else {
            Ok(())
        }
    }
}

async fn adaptive_runtime(
    mode: DriverMode,
) -> (
    page_runtime::PageRuntime,
    SessionId,
    PageId,
    Arc<Mutex<Vec<String>>>,
    tempfile::TempDir,
) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let journal = Arc::new(RecordingJournal {
        events: events.clone(),
        fail_on: None,
    });
    let workers = Arc::new(WorkerPool::new(
        1,
        Arc::new(FakeFactory {
            events: events.clone(),
            mode,
        }),
    ));
    let root = tempfile::tempdir().unwrap();
    let network = network_engine::NetworkPolicy {
        allow_loopback: true,
        ..Default::default()
    };
    let adaptive = page_runtime::AdaptivePageEngine::new(
        network_engine::EligibilityPolicy::new(network.clone()),
        network_engine::DirectHttpExecutor::new(network.clone()),
        artifact_store::ArtifactStore::new(root.path(), network.max_download_bytes, 16_384),
        network,
    );
    let runtime = page_runtime::PageRuntime::new_adaptive(journal, workers, None, adaptive);
    let session = SessionId::new();
    let page = runtime.open_browser(session.clone()).await.unwrap();
    (runtime, session, page.id, events, root)
}

async fn http_fixture(body: &'static str, content_type: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 2048];
        let _ = socket.read(&mut request).await.unwrap();
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    format!("http://{address}/")
}

fn completed_evidence(outcome: CommandOutcome) -> Vec<Evidence> {
    match outcome {
        CommandOutcome::Completed { evidence, .. } => evidence,
        other => panic!("unexpected outcome: {other:?}"),
    }
}

fn assert_one_lifecycle(events: &[String]) {
    for phase in [
        "accepted",
        "prepared",
        "executing",
        "verifying",
        "completed",
    ] {
        assert_eq!(
            events
                .iter()
                .filter(|event| event.as_str() == format!("journal:{phase}"))
                .count(),
            1
        );
    }
}

struct FakeFactory {
    events: Arc<Mutex<Vec<String>>>,
    mode: DriverMode,
}

#[async_trait]
impl WorkerFactory for FakeFactory {
    async fn launch(&self, _: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        Ok(Arc::new(FakeWorker {
            id: WorkerId::new(),
            profile: PathBuf::from("/tmp/fake-profile"),
            events: self.events.clone(),
            mode: self.mode,
        }))
    }
}

async fn runtime(
    mode: DriverMode,
    fail_on: Option<CommandPhase>,
) -> (
    page_runtime::PageRuntime,
    SessionId,
    PageId,
    Arc<Mutex<Vec<String>>>,
) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let journal = Arc::new(RecordingJournal {
        events: events.clone(),
        fail_on,
    });
    let workers = Arc::new(WorkerPool::new(
        8,
        Arc::new(FakeFactory {
            events: events.clone(),
            mode,
        }),
    ));
    let runtime = page_runtime::PageRuntime::new(journal, workers);
    let session = SessionId::new();
    let page = runtime.open_browser(session.clone()).await.unwrap();
    (runtime, session, page.id, events)
}

fn envelope(session: SessionId, page: PageId, command: PrimitiveCommand) -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: session,
        page_id: Some(page),
        deadline: Utc::now() + Duration::minutes(1),
        command,
    }
}

fn navigate() -> PrimitiveCommand {
    PrimitiveCommand::Navigate(NavigateCommand {
        url: "https://example.test/".into(),
        wait_until: WaitUntil::Interactive,
        timeout_ms: 1_000,
    })
}

fn driver_failure() -> CommandError {
    CommandError {
        code: ErrorCode::BrowserCommandFailed,
        message: "injected driver failure".into(),
        layer: ErrorLayer::Driver,
        retryable: true,
    }
}

#[tokio::test]
async fn prepares_durably_before_touching_browser() {
    let (runtime, session, page, events) = runtime(DriverMode::Succeed, None).await;
    let outcome = runtime.execute(envelope(session, page, navigate())).await;
    assert!(matches!(outcome, CommandOutcome::Completed { .. }));
    assert_eq!(
        &*events.lock().await,
        &[
            "journal:accepted",
            "journal:prepared",
            "journal:executing",
            "browser:navigate",
            "journal:verifying",
            "journal:completed",
        ]
    );
}

#[tokio::test]
async fn prepared_failure_never_touches_browser() {
    let (runtime, session, page, events) =
        runtime(DriverMode::Succeed, Some(CommandPhase::Prepared)).await;
    let outcome = runtime.execute(envelope(session, page, navigate())).await;
    assert!(matches!(outcome, CommandOutcome::RetryableFailure { .. }));
    assert!(!events
        .lock()
        .await
        .iter()
        .any(|event| event.starts_with("browser:")));
}

#[tokio::test]
async fn production_runtime_requires_matching_checkpoint_before_boundary_action() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let journal = Arc::new(RecordingJournal {
        events: events.clone(),
        fail_on: None,
    });
    let workers = Arc::new(WorkerPool::new(
        1,
        Arc::new(FakeFactory {
            events: events.clone(),
            mode: DriverMode::Succeed,
        }),
    ));
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let runtime = page_runtime::PageRuntime::new_with_checkpoints(journal, workers, store.clone());
    let session = SessionId::new();
    let page = runtime.open_browser(session.clone()).await.unwrap();
    let boundary = PrimitiveCommand::Click(ClickCommand {
        selector: "#submit".into(),
        target: None,
        boundary: true,
        expected_url: None,
    });
    let request = envelope(session.clone(), page.id.clone(), boundary.clone());

    let rejected = runtime.execute(request.clone()).await;
    assert!(matches!(rejected, CommandOutcome::Failed { error, .. }
        if error.code == ErrorCode::InvalidRequest && error.message.contains("checkpoint")));
    assert!(!events.lock().await.contains(&"browser:click".to_string()));

    store
        .save(&WorkflowCheckpoint {
            schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
            checkpoint_id: CheckpointId::new(),
            workflow_id: request.workflow_id.clone(),
            attempt_id: request.attempt_id.clone(),
            session_id: session,
            page_id: page.id,
            restart_url: "https://example.test/".into(),
            current_url: "https://example.test/".into(),
            cursor: None,
            boundary_command_id: Some(request.command_id.clone()),
            recovery_class: CommandClass::Boundary,
            invariants: Vec::new(),
            replayable_inputs: Vec::new(),
            evidence: Vec::new(),
            recovery_history: Vec::new(),
            created_at: Utc::now(),
        })
        .await
        .unwrap();
    let mut reused_request = request.clone();
    reused_request.command_id = CommandId::new();
    let accepted = runtime.execute(request).await;
    assert!(matches!(accepted, CommandOutcome::Completed { .. }));

    let reused = runtime.execute(reused_request).await;
    assert!(matches!(reused, CommandOutcome::Failed { error, .. }
        if error.message.contains("does not match")));
}

#[tokio::test]
async fn boundary_prepare_failure_is_safe_to_retry() {
    let (runtime, session, page, events) =
        runtime(DriverMode::Succeed, Some(CommandPhase::Prepared)).await;
    let outcome = runtime
        .execute(envelope(
            session,
            page,
            PrimitiveCommand::Click(ClickCommand {
                selector: "#submit".into(),
                target: None,
                boundary: true,
                expected_url: None,
            }),
        ))
        .await;
    assert!(matches!(outcome, CommandOutcome::RetryableFailure { .. }));
    assert!(!events
        .lock()
        .await
        .iter()
        .any(|event| event.starts_with("browser:")));
}

#[tokio::test]
async fn replayable_driver_failure_is_retryable() {
    let (runtime, session, page, _) = runtime(DriverMode::FailInspect, None).await;
    let outcome = runtime
        .execute(envelope(
            session,
            page,
            PrimitiveCommand::Inspect(InspectCommand::default()),
        ))
        .await;
    assert!(matches!(outcome, CommandOutcome::RetryableFailure { .. }));
}

#[tokio::test]
async fn boundary_driver_failure_needs_reconciliation() {
    let (runtime, session, page, _) = runtime(DriverMode::FailClick, None).await;
    let outcome = runtime
        .execute(envelope(
            session,
            page,
            PrimitiveCommand::Click(ClickCommand {
                selector: "#submit".into(),
                target: None,
                boundary: true,
                expected_url: None,
            }),
        ))
        .await;
    assert!(matches!(
        outcome,
        CommandOutcome::NeedsReconciliation { .. }
    ));
}

#[tokio::test]
async fn eligible_inspect_uses_http_without_browser_dispatch() {
    let url = http_fixture("<title>Fixture</title><p>Ada</p>", "text/html").await;
    let (runtime, session, page, events, _root) = adaptive_runtime(DriverMode::Succeed).await;
    runtime
        .set_url(&page, url.clone(), "interactive")
        .await
        .unwrap();
    events.lock().await.push(format!("url:{url}"));
    let evidence = completed_evidence(
        runtime
            .execute(envelope(
                session,
                page,
                PrimitiveCommand::Inspect(InspectCommand::default()),
            ))
            .await,
    );
    assert!(evidence.iter().any(|item| matches!(
        item,
        Evidence::ExecutionPath {
            path: ExecutionPath::DirectHttp,
            ..
        }
    )));
    let events = events.lock().await;
    assert!(!events.contains(&"browser:inspect".to_string()));
    assert_one_lifecycle(&events);
}

#[tokio::test]
async fn semantic_inspect_routes_directly_to_chromium() {
    let (runtime, session, page, events, _root) = adaptive_runtime(DriverMode::Succeed).await;
    let evidence = completed_evidence(
        runtime
            .execute(envelope(
                session,
                page,
                PrimitiveCommand::Inspect(InspectCommand {
                    selector: None,
                    target: Some(TargetSpec {
                        role: Some("button".into()),
                        ..Default::default()
                    }),
                    include_html: false,
                }),
            ))
            .await,
    );
    assert!(evidence.iter().any(|item| matches!(
        item,
        Evidence::ExecutionPath {
            path: ExecutionPath::Chromium,
            ..
        }
    )));
    let events = events.lock().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.as_str() == "browser:inspect")
            .count(),
        1
    );
    assert!(!events.contains(&"http:state".to_string()));
    assert_one_lifecycle(&events);
}

#[tokio::test]
async fn unproven_replayable_inspect_falls_back_once() {
    let url = http_fixture("<title>Fixture</title><p>Ada</p>", "text/html").await;
    let (runtime, session, page, events, _root) = adaptive_runtime(DriverMode::Succeed).await;
    runtime
        .set_url(&page, url.clone(), "interactive")
        .await
        .unwrap();
    events.lock().await.push(format!("url:{url}"));
    let evidence = completed_evidence(
        runtime
            .execute(envelope(
                session,
                page,
                PrimitiveCommand::Inspect(InspectCommand {
                    selector: Some(".missing".into()),
                    target: None,
                    include_html: false,
                }),
            ))
            .await,
    );
    assert!(evidence.iter().any(|item| matches!(
        item,
        Evidence::ExecutionPath {
            path: ExecutionPath::ChromiumFallback,
            ..
        }
    )));
    let events = events.lock().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.as_str() == "browser:inspect")
            .count(),
        1
    );
    assert_one_lifecycle(&events);
}

#[tokio::test]
async fn state_conflict_falls_back_without_committing_candidate_evidence() {
    let url = http_fixture("<title>Candidate</title><p>candidate-only</p>", "text/html").await;
    let (runtime, session, page, events, _root) = adaptive_runtime(DriverMode::StateConflict).await;
    runtime
        .set_url(&page, url.clone(), "interactive")
        .await
        .unwrap();
    events.lock().await.push(format!("url:{url}"));
    let evidence = completed_evidence(
        runtime
            .execute(envelope(
                session,
                page,
                PrimitiveCommand::Inspect(InspectCommand::default()),
            ))
            .await,
    );
    assert!(!evidence.iter().any(
        |item| matches!(item, Evidence::Inspection { text, .. } if text.contains("candidate-only"))
    ));
    assert!(evidence.iter().any(|item| matches!(
        item,
        Evidence::ExecutionPath {
            path: ExecutionPath::ChromiumFallback,
            ..
        }
    )));
    let events = events.lock().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.as_str() == "browser:inspect")
            .count(),
        1
    );
    assert_one_lifecycle(&events);
}

#[tokio::test]
async fn download_url_persists_then_returns_download_and_execution_evidence() {
    let url = http_fixture("durable-download", "application/octet-stream").await;
    let (runtime, session, page, events, root) = adaptive_runtime(DriverMode::Succeed).await;
    events.lock().await.push(format!("url:{url}"));
    let evidence = completed_evidence(
        runtime
            .execute(envelope(
                session.clone(),
                page,
                PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
                    url,
                    expected_content_type: Some("application/octet-stream".into()),
                    max_bytes: 1024,
                }),
            ))
            .await,
    );
    let (artifact_id, sha) = evidence
        .iter()
        .find_map(|item| match item {
            Evidence::Download { path, sha256, .. } => Some((path, sha256)),
            _ => None,
        })
        .unwrap();
    assert!(root
        .path()
        .join(session.0.to_string())
        .join(artifact_id)
        .is_dir());
    assert!(evidence.iter().any(|item| matches!(item, Evidence::ExecutionPath { path: ExecutionPath::DirectHttp, sha256: Some(execution_sha), .. } if execution_sha == sha)));
    let events = events.lock().await;
    assert!(!events.iter().any(|event| event.starts_with("browser:")));
    assert_one_lifecycle(&events);
}

#[tokio::test]
async fn download_url_content_type_mismatch_fails_closed_without_browser_dispatch() {
    let url = http_fixture("not-the-expected-type", "text/plain").await;
    let (runtime, session, page, events, _root) = adaptive_runtime(DriverMode::Succeed).await;
    events.lock().await.push(format!("url:{url}"));
    let outcome = runtime
        .execute(envelope(
            session,
            page,
            PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
                url,
                expected_content_type: Some("application/octet-stream".into()),
                max_bytes: 1024,
            }),
        ))
        .await;
    assert!(
        matches!(outcome, CommandOutcome::Failed { error, .. } if error.code == ErrorCode::HttpEquivalenceUnproven)
    );
    let events = events.lock().await;
    assert!(!events.iter().any(|event| event.starts_with("browser:")));
    assert!(!events.contains(&"http:commit".to_string()));
}

#[tokio::test]
async fn download_state_commit_failure_removes_pending_artifact_and_leaks_no_evidence() {
    let url = http_fixture("guarded-download", "application/octet-stream").await;
    let (runtime, session, page, events, root) = adaptive_runtime(DriverMode::CommitFail).await;
    events.lock().await.push(format!("url:{url}"));
    let outcome = runtime
        .execute(envelope(
            session.clone(),
            page,
            PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
                url,
                expected_content_type: None,
                max_bytes: 1024,
            }),
        ))
        .await;
    assert!(matches!(outcome, CommandOutcome::Failed { .. }));
    let session_dir = root.path().join(session.0.to_string());
    assert!(!session_dir.exists() || std::fs::read_dir(session_dir).unwrap().next().is_none());
    assert!(!format!("{outcome:?}").contains("Download"));
}

#[tokio::test]
async fn download_state_commit_cancellation_removes_pending_artifact() {
    let url = http_fixture("cancelled-download", "application/octet-stream").await;
    let (runtime, session, page, events, root) = adaptive_runtime(DriverMode::CommitPause).await;
    events.lock().await.push(format!("url:{url}"));
    let runtime_for_task = runtime.clone();
    let session_for_task = session.clone();
    let handle = tokio::spawn(async move {
        runtime_for_task
            .execute(envelope(
                session_for_task,
                page,
                PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
                    url,
                    expected_content_type: None,
                    max_bytes: 1024,
                }),
            ))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if events.lock().await.contains(&"http:commit".to_string()) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("state commit was not reached");
    assert!(events.lock().await.contains(&"http:commit".to_string()));
    handle.abort();
    let _ = handle.await;
    let session_dir = root.path().join(session.0.to_string());
    assert!(!session_dir.exists() || std::fs::read_dir(session_dir).unwrap().next().is_none());
}

#[tokio::test]
async fn mutating_and_boundary_commands_never_call_http() {
    let (runtime, session, page, events, _root) = adaptive_runtime(DriverMode::Succeed).await;
    let outcome = runtime
        .execute(envelope(
            session,
            page,
            PrimitiveCommand::Click(ClickCommand {
                selector: "#safe".into(),
                target: None,
                boundary: false,
                expected_url: None,
            }),
        ))
        .await;
    assert!(matches!(outcome, CommandOutcome::Completed { .. }));
    let events = events.lock().await;
    assert!(!events.contains(&"http:state".to_string()));
    assert_one_lifecycle(&events);
}

#[tokio::test]
async fn terminal_policy_denial_never_calls_chromium() {
    let (runtime, session, page, events, _root) = adaptive_runtime(DriverMode::Succeed).await;
    runtime
        .set_url(&page, "file:///secret".into(), "interactive")
        .await
        .unwrap();
    let outcome = runtime
        .execute(envelope(
            session,
            page,
            PrimitiveCommand::Inspect(InspectCommand::default()),
        ))
        .await;
    assert!(matches!(outcome, CommandOutcome::PolicyDenied { .. }));
    let events = events.lock().await;
    assert!(!events.iter().any(|event| event.starts_with("browser:")));
    assert!(!events.contains(&"http:state".to_string()));
}
