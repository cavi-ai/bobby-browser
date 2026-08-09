use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use checkpoint_store::CheckpointStore;
use chrono::{Duration, Utc};
use tokio::sync::Mutex;
use types::{
    AttemptId, CheckpointId, ClickCommand, CommandClass, CommandEnvelope, CommandError, CommandId,
    CommandOutcome, CommandPhase, DownloadUrlCommand, ErrorCode, ErrorLayer, Evidence,
    ExecutionPath, ExecutionReason, FollowIntent, InspectCommand, IntentCommand, IntentHints,
    NavigateCommand, PageId, PrimitiveCommand, RuntimeCommand, SessionId, SubmitAndVerifyIntent,
    TargetSpec, TextMatch, TypeTextCommand, WaitCondition, WaitForCommand, WaitUntil, WorkerId,
    WorkflowCheckpoint, WorkflowId,
};
use worker_pool::{BrowserWorker, WorkerFactory, WorkerPool};
use workflow_journal::{
    CommandJournal, JournalError, JournalRecord, JournalScan, JsonlJournal, PreparedResult,
};

#[derive(Clone, Copy)]
enum DriverMode {
    Succeed,
    FailInspect,
    SlowInspect,
    InspectMismatch,
    FailNavigate,
    FailClick,
    ClickTargetNotFound,
    WaitTimeout,
    TargetDetached,
    DeadOnOpen,
    StateConflict,
    CommitFail,
    CommitPause,
}

struct RecordingJournal {
    events: Arc<Mutex<Vec<String>>>,
    fail_on: Option<CommandPhase>,
    pause_on: Option<CommandPhase>,
    paused: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
}

struct RecordingPhaseObserver(Arc<Mutex<Vec<CommandPhase>>>);

#[async_trait]
impl page_runtime::ExecutionPhaseObserver for RecordingPhaseObserver {
    async fn durable_phase_reached(&self, phase: CommandPhase) {
        self.0.lock().await.push(phase);
    }
}

struct RecoveryJournal {
    records: Vec<JournalRecord>,
}

struct DurablePausingJournal {
    inner: JsonlJournal,
    paused: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl CommandJournal for DurablePausingJournal {
    async fn append(&self, record: JournalRecord) -> Result<(), JournalError> {
        let phase = record.phase;
        self.inner.append(record).await?;
        if phase == CommandPhase::ResultPrepared {
            self.paused.notify_one();
            self.resume.notified().await;
        }
        Ok(())
    }

    async fn history(&self, id: CommandId) -> Result<JournalScan, JournalError> {
        self.inner.history(id).await
    }
}

#[async_trait]
impl CommandJournal for RecoveryJournal {
    async fn append(&self, _: JournalRecord) -> Result<(), JournalError> {
        Ok(())
    }
    async fn history(&self, _: CommandId) -> Result<JournalScan, JournalError> {
        Ok(JournalScan {
            records: self.records.clone(),
            torn_tail: false,
            incompatible_records: 0,
        })
    }
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
        if self.pause_on == Some(record.phase) {
            self.paused.notify_one();
            self.resume.notified().await;
        }
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
        if matches!(self.mode, DriverMode::DeadOnOpen) {
            return Err(CommandError {
                code: ErrorCode::BrowserCommandFailed,
                message: "send failed because receiver is gone".into(),
                layer: ErrorLayer::Driver,
                retryable: true,
            });
        }
        Ok(())
    }
    async fn navigate(
        &self,
        _: &PageId,
        command: &NavigateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.events.lock().await.push("browser:navigate".into());
        if matches!(self.mode, DriverMode::FailNavigate) {
            return Err(driver_failure());
        }
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
        if matches!(self.mode, DriverMode::SlowInspect) {
            tokio::time::sleep(StdDuration::from_secs(30)).await;
        }
        if matches!(self.mode, DriverMode::InspectMismatch) {
            return Ok(vec![Evidence::Inspection {
                selector: command.selector.clone(),
                url: "https://example.test/".into(),
                title: "Fixture".into(),
                text: "not-the-typed-value".into(),
                html: None,
            }]);
        }
        if matches!(self.mode, DriverMode::FailInspect) {
            return Err(driver_failure());
        }
        let text = if command.selector.as_deref() == Some("select[aria-label='Plan']")
            && self
                .events
                .lock()
                .await
                .iter()
                .any(|event| event == "browser:type_text")
        {
            "pro"
        } else {
            command.selector.as_deref().map_or("page", |_| "Ada")
        };
        Ok(vec![Evidence::Inspection {
            selector: command.selector.clone(),
            url: "https://example.test/".into(),
            title: "Fixture".into(),
            text: text.into(),
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
        if matches!(self.mode, DriverMode::ClickTargetNotFound) {
            return Err(CommandError {
                code: ErrorCode::TargetNotFound,
                message: "no target candidate matched".into(),
                layer: ErrorLayer::Driver,
                retryable: false,
            });
        }
        if matches!(self.mode, DriverMode::TargetDetached) {
            return Err(CommandError {
                code: ErrorCode::TargetDetached,
                message: "the browser target is gone (crashed or closed); re-list pages or recover the session before retrying".into(),
                layer: ErrorLayer::Driver,
                retryable: true,
            });
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
    async fn collect_candidates(
        &self,
        _: &PageId,
        target: &TargetSpec,
    ) -> Result<Vec<dom_engine::Candidate>, CommandError> {
        self.events
            .lock()
            .await
            .push("browser:collect_candidates".into());
        let name = target
            .accessible_name
            .clone()
            .or_else(|| match &target.text {
                Some(TextMatch::Contains(text) | TextMatch::Exact(text)) => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "Submit".into());
        Ok(vec![dom_engine::Candidate {
            id: "submit".into(),
            css: Some("#submit".into()),
            test_id: None,
            role: target.role.clone().or_else(|| Some("button".into())),
            name: Some(name.clone()),
            label: None,
            text: name,
            attributes: Default::default(),
            state: dom_engine::CandidateState {
                attached: true,
                visible: true,
                enabled: true,
            },
            frame_path: Vec::new(),
        }])
    }
    async fn wait_for(
        &self,
        _: &PageId,
        command: &WaitForCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.events.lock().await.push("browser:wait_for".into());
        if matches!(self.mode, DriverMode::WaitTimeout) {
            return Err(CommandError {
                code: ErrorCode::WaitConditionTimedOut,
                message: "wait condition was not satisfied within 1000ms".into(),
                layer: ErrorLayer::Driver,
                retryable: false,
            });
        }
        Ok(vec![Evidence::Wait {
            condition: command.condition.clone(),
            elapsed_ms: 1,
            observations: 1,
            excluded_classes: Vec::new(),
            observed: None,
        }])
    }
    async fn close(&self) -> Result<(), CommandError> {
        Ok(())
    }
    fn supports_http_state(&self) -> bool {
        true
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
    adaptive_runtime_with_failure(mode, None).await
}

async fn adaptive_runtime_with_failure(
    mode: DriverMode,
    fail_on: Option<CommandPhase>,
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
        fail_on,
        pause_on: None,
        paused: Arc::new(tokio::sync::Notify::new()),
        resume: Arc::new(tokio::sync::Notify::new()),
    });
    let workers = Arc::new(WorkerPool::new(
        1,
        Arc::new(FakeFactory {
            events: events.clone(),
            mode,
            launches: Default::default(),
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
    )
    .with_downloads_root(root.path().join("downloads"));
    let runtime = page_runtime::PageRuntime::new_adaptive(journal, workers, None, adaptive);
    let session = SessionId::new();
    let page = runtime.open_browser(session.clone()).await.unwrap();
    (runtime, session, page.id, events, root)
}

async fn adaptive_runtime_paused(
    phase: CommandPhase,
) -> (
    page_runtime::PageRuntime,
    SessionId,
    PageId,
    Arc<Mutex<Vec<String>>>,
    tempfile::TempDir,
    Arc<tokio::sync::Notify>,
    Arc<tokio::sync::Notify>,
) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let paused = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let journal = Arc::new(RecordingJournal {
        events: events.clone(),
        fail_on: None,
        pause_on: Some(phase),
        paused: paused.clone(),
        resume: resume.clone(),
    });
    let workers = Arc::new(WorkerPool::new(
        1,
        Arc::new(FakeFactory {
            events: events.clone(),
            mode: DriverMode::Succeed,
            launches: Default::default(),
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
    )
    .with_downloads_root(root.path().join("downloads"));
    let runtime = page_runtime::PageRuntime::new_adaptive(journal, workers, None, adaptive);
    let session = SessionId::new();
    let page = runtime.open_browser(session.clone()).await.unwrap();
    (runtime, session, page.id, events, root, paused, resume)
}

async fn http_fixture(body: &'static str, content_type: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // Serve every connection: a test may fetch the fixture several times.
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = [0; 2048];
            if socket.read(&mut request).await.is_err() {
                continue;
            }
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            if socket.write_all(response.as_bytes()).await.is_err() {
                continue;
            }
        }
    });
    format!("http://{address}/")
}

async fn counted_http_fixture() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let requests = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let observed = requests.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = [0; 2048];
            if socket.read(&mut request).await.is_err() {
                continue;
            }
            observed.fetch_add(1, Ordering::SeqCst);
            let response = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 7\r\nConnection: close\r\n\r\nprivate";
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    (format!("http://{address}/"), requests)
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

async fn assert_single_download_readable(root: &Path, session: &SessionId, expected: &[u8]) {
    let mut entries = std::fs::read_dir(root.join(session.0.to_string())).unwrap();
    let entry = entries.next().expect("one durable artifact").unwrap();
    assert!(entries.next().is_none(), "expected exactly one artifact");
    let artifact_id = entry.file_name().to_string_lossy().into_owned();
    let store = artifact_store::ArtifactStore::new(root, 1024 * 1024, 16_384);
    assert_eq!(store.get(session, &artifact_id).await.unwrap(), expected);
}

struct FakeFactory {
    events: Arc<Mutex<Vec<String>>>,
    mode: DriverMode,
    launches: Arc<std::sync::atomic::AtomicUsize>,
}

struct DeadThenFailFactory {
    events: Arc<Mutex<Vec<String>>>,
    launches: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl WorkerFactory for DeadThenFailFactory {
    async fn launch(&self, _: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        if self
            .launches
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 0
        {
            return Ok(Arc::new(FakeWorker {
                id: WorkerId::new(),
                profile: PathBuf::from("/tmp/dead-then-fail-profile"),
                events: self.events.clone(),
                mode: DriverMode::DeadOnOpen,
            }));
        }
        Err(CommandError {
            code: ErrorCode::BrowserLaunchFailed,
            message: "injected replacement launch failure".into(),
            layer: ErrorLayer::Driver,
            retryable: true,
        })
    }
}

#[async_trait]
impl WorkerFactory for FakeFactory {
    async fn launch(&self, _: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        // DeadOnOpen: only the first launch is dead; replacements are healthy,
        // so a revived session can open pages again.
        let mode = if matches!(self.mode, DriverMode::DeadOnOpen)
            && self
                .launches
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                > 0
        {
            DriverMode::Succeed
        } else {
            self.mode
        };
        Ok(Arc::new(FakeWorker {
            id: WorkerId::new(),
            profile: PathBuf::from("/tmp/fake-profile"),
            events: self.events.clone(),
            mode,
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
        pause_on: None,
        paused: Arc::new(tokio::sync::Notify::new()),
        resume: Arc::new(tokio::sync::Notify::new()),
    });
    let workers = Arc::new(WorkerPool::new(
        8,
        Arc::new(FakeFactory {
            events: events.clone(),
            mode,
            launches: Default::default(),
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
        command: RuntimeCommand::Primitive(command),
    }
}

fn intent_envelope(session: SessionId, page: PageId, command: IntentCommand) -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: session,
        page_id: Some(page),
        deadline: Utc::now() + Duration::minutes(1),
        command: RuntimeCommand::Intent(command),
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
async fn missing_registered_page_is_not_found_while_other_page_validation_stays_invalid_request() {
    let (runtime, session, page, _) = runtime(DriverMode::Succeed, None).await;

    let missing = runtime
        .execute(envelope(session.clone(), PageId::new(), navigate()))
        .await;
    assert!(matches!(
        missing,
        CommandOutcome::Failed { error, .. }
            if error.code == ErrorCode::NotFound
                && error.layer == ErrorLayer::Workflow
                && !error.retryable
    ));

    let wrong_session = runtime
        .execute(envelope(SessionId::new(), page.clone(), navigate()))
        .await;
    assert!(matches!(
        wrong_session,
        CommandOutcome::Failed { error, .. }
            if error.code == ErrorCode::InvalidRequest
    ));

    let mut missing_page_id = envelope(session, page, navigate());
    missing_page_id.page_id = None;
    let missing_page_id = runtime.execute(missing_page_id).await;
    assert!(matches!(
        missing_page_id,
        CommandOutcome::Failed { error, .. }
            if error.code == ErrorCode::InvalidRequest
    ));
}

#[tokio::test]
async fn raw_css_select_type_text_verifies_the_exact_option_value() {
    let (runtime, session, page, events) = runtime(DriverMode::Succeed, None).await;
    let outcome = runtime
        .execute(envelope(
            session,
            page,
            PrimitiveCommand::TypeText(TypeTextCommand {
                selector: "select[aria-label='Plan']".into(),
                target: None,
                value: "pro".into(),
                clear_first: true,
                expected_url: None,
            }),
        ))
        .await;

    assert!(matches!(outcome, CommandOutcome::Completed { .. }));
    let events = events.lock().await;
    assert!(events.contains(&"browser:type_text".to_string()));
    assert!(events.contains(&"browser:inspect".to_string()));
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
async fn production_phase_observer_fires_only_after_each_durable_lifecycle_append() {
    let (runtime, session, page, events) = runtime(DriverMode::Succeed, None).await;
    let phases = Arc::new(Mutex::new(Vec::new()));
    let runtime =
        runtime.with_execution_phase_observer(Arc::new(RecordingPhaseObserver(phases.clone())));

    let outcome = runtime.execute(envelope(session, page, navigate())).await;

    assert!(matches!(outcome, CommandOutcome::Completed { .. }));
    assert_eq!(
        *phases.lock().await,
        [
            CommandPhase::Accepted,
            CommandPhase::Prepared,
            CommandPhase::Executing,
            CommandPhase::Verifying,
        ]
    );
    let events = events.lock().await;
    for phase in ["accepted", "prepared", "executing", "verifying"] {
        assert!(events
            .iter()
            .any(|event| event == &format!("journal:{phase}")));
    }
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
        pause_on: None,
        paused: Arc::new(tokio::sync::Notify::new()),
        resume: Arc::new(tokio::sync::Notify::new()),
    });
    let workers = Arc::new(WorkerPool::new(
        1,
        Arc::new(FakeFactory {
            events: events.clone(),
            mode: DriverMode::Succeed,
            launches: Default::default(),
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
            recovery_receipts: Vec::new(),
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
async fn submit_and_verify_requires_matching_checkpoint_before_boundary_act() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let journal = Arc::new(RecordingJournal {
        events: events.clone(),
        fail_on: None,
        pause_on: None,
        paused: Arc::new(tokio::sync::Notify::new()),
        resume: Arc::new(tokio::sync::Notify::new()),
    });
    let workers = Arc::new(WorkerPool::new(
        1,
        Arc::new(FakeFactory {
            events: events.clone(),
            mode: DriverMode::Succeed,
            launches: Default::default(),
        }),
    ));
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let runtime = page_runtime::PageRuntime::new_with_checkpoints(journal, workers, store.clone());
    let session = SessionId::new();
    let page = runtime.open_browser(session.clone()).await.unwrap();
    let submit = IntentCommand::SubmitAndVerify(SubmitAndVerifyIntent {
        purpose: "Submit application".into(),
        hints: IntentHints {
            role: Some("button".into()),
            ..IntentHints::default()
        },
        expected_state: WaitForCommand {
            condition: WaitCondition::Url {
                matcher: TextMatch::Contains("/thanks".into()),
            },
            timeout_ms: 5_000,
        },
    });
    assert_eq!(submit.class(), CommandClass::Boundary);
    let request = intent_envelope(session.clone(), page.id.clone(), submit);

    let rejected = runtime.execute(request.clone()).await;
    assert!(matches!(rejected, CommandOutcome::Failed { error, .. }
        if error.code == ErrorCode::InvalidRequest && error.message.contains("checkpoint")));
    let observed = events.lock().await.clone();
    assert!(!observed.iter().any(|event| event.contains("browser:")));

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
            recovery_receipts: Vec::new(),
            created_at: Utc::now(),
        })
        .await
        .unwrap();

    let accepted = runtime.execute(request).await;
    assert!(
        matches!(accepted, CommandOutcome::Completed { .. }),
        "{accepted:?}"
    );
    let observed = events.lock().await.clone();
    assert!(observed
        .iter()
        .any(|event| event == "browser:collect_candidates"));
    assert!(observed.iter().any(|event| event == "browser:click"));
    assert!(observed.iter().any(|event| event == "browser:wait_for"));
}

fn follow(purpose: &str, boundary: bool) -> IntentCommand {
    IntentCommand::Follow(FollowIntent {
        purpose: purpose.into(),
        hints: IntentHints {
            role: Some("link".into()),
            ..IntentHints::default()
        },
        expected_destination: WaitForCommand {
            condition: WaitCondition::Url {
                matcher: TextMatch::Contains("/details".into()),
            },
            timeout_ms: 5_000,
        },
        boundary,
    })
}

#[tokio::test]
async fn follow_with_boundary_true_requires_matching_checkpoint_before_boundary_act() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let journal = Arc::new(RecordingJournal {
        events: events.clone(),
        fail_on: None,
        pause_on: None,
        paused: Arc::new(tokio::sync::Notify::new()),
        resume: Arc::new(tokio::sync::Notify::new()),
    });
    let workers = Arc::new(WorkerPool::new(
        1,
        Arc::new(FakeFactory {
            events: events.clone(),
            mode: DriverMode::Succeed,
            launches: Default::default(),
        }),
    ));
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let runtime = page_runtime::PageRuntime::new_with_checkpoints(journal, workers, store.clone());
    let session = SessionId::new();
    let page = runtime.open_browser(session.clone()).await.unwrap();
    let intent = follow("Sign out", true);
    assert_eq!(intent.class(), CommandClass::Boundary);
    let request = intent_envelope(session.clone(), page.id.clone(), intent);

    let rejected = runtime.execute(request.clone()).await;
    assert!(matches!(rejected, CommandOutcome::Failed { error, .. }
        if error.code == ErrorCode::InvalidRequest && error.message.contains("checkpoint")));
    let observed = events.lock().await.clone();
    assert!(!observed.iter().any(|event| event.contains("browser:")));

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
            recovery_receipts: Vec::new(),
            created_at: Utc::now(),
        })
        .await
        .unwrap();

    let accepted = runtime.execute(request).await;
    assert!(
        matches!(accepted, CommandOutcome::Completed { .. }),
        "{accepted:?}"
    );
    let observed = events.lock().await.clone();
    assert!(observed
        .iter()
        .any(|event| event == "browser:collect_candidates"));
    assert!(observed.iter().any(|event| event == "browser:click"));
    assert!(observed.iter().any(|event| event == "browser:wait_for"));
}

#[tokio::test]
async fn follow_with_boundary_false_runs_without_any_pre_established_checkpoint() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let journal = Arc::new(RecordingJournal {
        events: events.clone(),
        fail_on: None,
        pause_on: None,
        paused: Arc::new(tokio::sync::Notify::new()),
        resume: Arc::new(tokio::sync::Notify::new()),
    });
    let workers = Arc::new(WorkerPool::new(
        1,
        Arc::new(FakeFactory {
            events: events.clone(),
            mode: DriverMode::Succeed,
            launches: Default::default(),
        }),
    ));
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let runtime = page_runtime::PageRuntime::new_with_checkpoints(journal, workers, store);
    let session = SessionId::new();
    let page = runtime.open_browser(session.clone()).await.unwrap();
    let intent = follow("Details", false);
    assert_eq!(intent.class(), CommandClass::Reconciliable);
    let request = intent_envelope(session, page.id, intent);

    let accepted = runtime.execute(request).await;
    assert!(
        matches!(accepted, CommandOutcome::Completed { .. }),
        "{accepted:?}"
    );
    let observed = events.lock().await.clone();
    assert!(observed
        .iter()
        .any(|event| event == "browser:collect_candidates"));
    assert!(observed.iter().any(|event| event == "browser:click"));
    assert!(observed.iter().any(|event| event == "browser:wait_for"));
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

/// A dead browser must not wedge the session: open_browser invalidates the
/// dead worker and retries on a fresh one instead of returning `internal`.
#[tokio::test]
async fn open_browser_revives_a_dead_worker_once() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let journal = Arc::new(RecordingJournal {
        events: events.clone(),
        fail_on: None,
        pause_on: None,
        paused: Arc::new(tokio::sync::Notify::new()),
        resume: Arc::new(tokio::sync::Notify::new()),
    });
    let workers = Arc::new(WorkerPool::new(
        8,
        Arc::new(FakeFactory {
            events: events.clone(),
            mode: DriverMode::DeadOnOpen,
            launches: Default::default(),
        }),
    ));
    let runtime = page_runtime::PageRuntime::new(journal, workers);
    let session = SessionId::new();
    runtime
        .open_browser(session.clone())
        .await
        .expect("open_browser must revive a dead worker");
    runtime
        .open_browser(session)
        .await
        .expect("the revived worker keeps serving the session");
}

#[tokio::test]
async fn failed_replacement_launch_does_not_leave_a_registered_page() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let journal = Arc::new(RecordingJournal {
        events: events.clone(),
        fail_on: None,
        pause_on: None,
        paused: Arc::new(tokio::sync::Notify::new()),
        resume: Arc::new(tokio::sync::Notify::new()),
    });
    let workers = Arc::new(WorkerPool::new(
        8,
        Arc::new(DeadThenFailFactory {
            events,
            launches: std::sync::atomic::AtomicUsize::new(0),
        }),
    ));
    let runtime = page_runtime::PageRuntime::new(journal, workers);
    let session = SessionId::new();

    runtime
        .open_browser(session.clone())
        .await
        .expect_err("replacement launch must fail");

    assert!(
        runtime.remove_session_pages(&session).await.is_empty(),
        "a failed open must not remain registered"
    );
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

/// A Boundary command whose target never resolved never reached the browser:
/// that is a plain failure, not a reconciliation stop.
#[tokio::test]
async fn boundary_pre_effect_resolution_failure_is_failed_not_needs_reconciliation() {
    let (runtime, session, page, _) = runtime(DriverMode::ClickTargetNotFound, None).await;
    let outcome = runtime
        .execute(envelope(
            session,
            page,
            PrimitiveCommand::Click(ClickCommand {
                selector: "#missing".into(),
                target: None,
                boundary: true,
                expected_url: None,
            }),
        ))
        .await;
    assert!(matches!(outcome, CommandOutcome::Failed { error, .. }
        if error.code == ErrorCode::TargetNotFound));
}

/// A Boundary act whose postcondition wait times out already reached the
/// browser; keep it as plain `failed` (inspect/retry guidance) instead of
/// the never-retry `needsReconciliation` path.
#[tokio::test]
async fn boundary_wait_timeout_is_failed_not_needs_reconciliation() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let journal = Arc::new(RecordingJournal {
        events: events.clone(),
        fail_on: None,
        pause_on: None,
        paused: Arc::new(tokio::sync::Notify::new()),
        resume: Arc::new(tokio::sync::Notify::new()),
    });
    let workers = Arc::new(WorkerPool::new(
        1,
        Arc::new(FakeFactory {
            events: events.clone(),
            mode: DriverMode::WaitTimeout,
            launches: Default::default(),
        }),
    ));
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let runtime = page_runtime::PageRuntime::new_with_checkpoints(journal, workers, store.clone());
    let session = SessionId::new();
    let page = runtime.open_browser(session.clone()).await.unwrap();
    let submit = IntentCommand::SubmitAndVerify(SubmitAndVerifyIntent {
        purpose: "Save priority".into(),
        hints: IntentHints {
            role: Some("button".into()),
            accessible_name: Some("Save".into()),
            ..IntentHints::default()
        },
        expected_state: WaitForCommand {
            condition: WaitCondition::Text {
                target: Box::new(TargetSpec {
                    role: Some("main".into()),
                    ..TargetSpec::default()
                }),
                matcher: TextMatch::Contains("Priority saved".into()),
            },
            timeout_ms: 1_000,
        },
    });
    let request = intent_envelope(session.clone(), page.id.clone(), submit);
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
            recovery_receipts: Vec::new(),
            created_at: Utc::now(),
        })
        .await
        .unwrap();
    let outcome = runtime.execute(request).await;
    assert!(matches!(outcome, CommandOutcome::Failed { error, .. }
        if error.code == ErrorCode::WaitConditionTimedOut));
}

#[tokio::test]
async fn boundary_target_detached_is_retryable_not_needs_reconciliation() {
    let (runtime, session, page, _) = runtime(DriverMode::TargetDetached, None).await;
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
        CommandOutcome::RetryableFailure { error, .. }
            if error.code == ErrorCode::TargetDetached
    ));
}

#[tokio::test]
async fn inspect_of_a_mutated_page_uses_the_browser_not_a_refetch() {
    let url = http_fixture("<title>Fixture</title><p>Ada</p>", "text/html").await;
    let (runtime, session, page, events, _root) = adaptive_runtime(DriverMode::Succeed).await;
    runtime
        .set_url(&page, url.clone(), "interactive")
        .await
        .unwrap();
    // The fake worker's HTTP mirror reads its URL from this event.
    events.lock().await.push(format!("url:{url}"));
    // Untainted: the direct-HTTP read optimization applies.
    let first = runtime
        .execute(envelope(
            session.clone(),
            page.clone(),
            PrimitiveCommand::Inspect(InspectCommand::default()),
        ))
        .await;
    let evidence = completed_evidence(first);
    assert!(evidence.iter().any(|item| matches!(
        item,
        Evidence::ExecutionPath {
            path: ExecutionPath::DirectHttp,
            ..
        }
    )));
    // A mutating command taints the page.
    let click = runtime
        .execute(envelope(
            session.clone(),
            page.clone(),
            PrimitiveCommand::Click(ClickCommand {
                selector: "#safe".into(),
                target: None,
                boundary: false,
                expected_url: None,
            }),
        ))
        .await;
    assert!(matches!(click, CommandOutcome::Completed { .. }));
    // The next whole-page inspect must read the live DOM, not a refetch.
    let evidence = completed_evidence(
        runtime
            .execute(envelope(
                session.clone(),
                page.clone(),
                PrimitiveCommand::Inspect(InspectCommand::default()),
            ))
            .await,
    );
    assert!(evidence.iter().any(|item| matches!(
        item,
        Evidence::ExecutionPath {
            path: ExecutionPath::Chromium,
            reason: ExecutionReason::PageMutated,
            ..
        }
    )));
    // Navigation replaces the DOM: the taint clears and HTTP reads resume.
    runtime
        .set_url(&page, url.clone(), "interactive")
        .await
        .unwrap();
    let navigate = runtime
        .execute(envelope(
            session.clone(),
            page.clone(),
            PrimitiveCommand::Navigate(NavigateCommand {
                url: url.clone(),
                wait_until: types::WaitUntil::Interactive,
                timeout_ms: 30_000,
            }),
        ))
        .await;
    assert!(matches!(navigate, CommandOutcome::Completed { .. }));
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
    let _ = events;
}

#[tokio::test]
async fn failed_mutation_taints_the_page_before_the_next_inspect() {
    let url = http_fixture(
        "<title>Fixture</title><p>stale server copy</p>",
        "text/html",
    )
    .await;
    let (runtime, session, page, events, _root) = adaptive_runtime(DriverMode::FailClick).await;
    runtime
        .set_url(&page, url.clone(), "interactive")
        .await
        .unwrap();
    events.lock().await.push(format!("url:{url}"));

    let click = runtime
        .execute(envelope(
            session.clone(),
            page.clone(),
            PrimitiveCommand::Click(ClickCommand {
                selector: "#may-have-fired".into(),
                target: None,
                boundary: false,
                expected_url: None,
            }),
        ))
        .await;
    assert!(!matches!(click, CommandOutcome::Completed { .. }));

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
            path: ExecutionPath::Chromium,
            reason: ExecutionReason::PageMutated,
            ..
        }
    )));
}

#[tokio::test]
async fn failed_navigation_taints_the_page_before_the_next_inspect() {
    let url = http_fixture("<title>Fixture</title><p>old document</p>", "text/html").await;
    let (runtime, session, page, events, _root) = adaptive_runtime(DriverMode::FailNavigate).await;
    runtime
        .set_url(&page, url.clone(), "interactive")
        .await
        .unwrap();
    events.lock().await.push(format!("url:{url}"));

    let navigate = runtime
        .execute(envelope(
            session.clone(),
            page.clone(),
            PrimitiveCommand::Navigate(NavigateCommand {
                url: url.clone(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 30_000,
            }),
        ))
        .await;
    assert!(!matches!(navigate, CommandOutcome::Completed { .. }));

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
            path: ExecutionPath::Chromium,
            reason: ExecutionReason::PageMutated,
            ..
        }
    )));
}

#[tokio::test]
async fn a_side_band_download_does_not_taint_the_page() {
    let page_url = http_fixture("<title>Fixture</title><p>Ada</p>", "text/html").await;
    let download_url = http_fixture("durable-download", "application/octet-stream").await;
    let (runtime, session, page, events, _root) = adaptive_runtime(DriverMode::Succeed).await;
    runtime
        .set_url(&page, page_url.clone(), "interactive")
        .await
        .unwrap();
    events.lock().await.push(format!("url:{page_url}"));
    // A download fetches beside the page; it leaves the document untouched.
    let download = runtime
        .execute(envelope(
            session.clone(),
            page.clone(),
            PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
                url: download_url,
                expected_content_type: Some("application/octet-stream".into()),
                max_bytes: 1024,
                save_as: None,
            }),
        ))
        .await;
    assert!(matches!(download, CommandOutcome::Completed { .. }));
    let evidence = completed_evidence(
        runtime
            .execute(envelope(
                session,
                page,
                PrimitiveCommand::Inspect(InspectCommand::default()),
            ))
            .await,
    );
    assert!(
        evidence.iter().any(|item| matches!(
            item,
            Evidence::ExecutionPath {
                path: ExecutionPath::DirectHttp,
                ..
            }
        )),
        "a download must not push the next whole-page read onto a stale DOM"
    );
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
    let prepared = events
        .iter()
        .position(|event| event == "journal:resultprepared")
        .expect("direct inspection must durably prepare before state commit");
    let committed = events
        .iter()
        .position(|event| event == "http:commit")
        .expect("direct inspection must commit response state");
    assert!(prepared < committed);
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
async fn state_conflict_after_prepared_inspect_requires_reconciliation_without_replay() {
    let url = http_fixture("<title>Candidate</title><p>candidate-only</p>", "text/html").await;
    let (runtime, session, page, events, _root) = adaptive_runtime(DriverMode::StateConflict).await;
    runtime
        .set_url(&page, url.clone(), "interactive")
        .await
        .unwrap();
    events.lock().await.push(format!("url:{url}"));
    let outcome = runtime
        .execute(envelope(
            session,
            page,
            PrimitiveCommand::Inspect(InspectCommand::default()),
        ))
        .await;
    assert!(
        matches!(outcome, CommandOutcome::NeedsReconciliation { evidence, .. }
        if evidence.iter().any(|item| matches!(item, Evidence::Inspection { text, .. } if text.contains("candidate-only"))))
    );
    let events = events.lock().await;
    assert!(!events.contains(&"browser:inspect".to_string()));
    assert!(events.contains(&"journal:resultprepared".to_string()));
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
                    save_as: None,
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
    let prepared = events
        .iter()
        .position(|event| event == "journal:resultprepared")
        .unwrap();
    let committed = events
        .iter()
        .position(|event| event == "http:commit")
        .unwrap();
    let completed = events
        .iter()
        .position(|event| event == "journal:completed")
        .unwrap();
    assert!(prepared < committed && committed < completed);
    assert_one_lifecycle(&events);
}

#[tokio::test]
async fn download_url_materializes_requested_file_below_downloads_root() {
    let url = http_fixture("customer,priority\nAtlas Labs,high\n", "text/csv").await;
    let (runtime, session, page, _events, root) = adaptive_runtime(DriverMode::Succeed).await;
    let destination = root.path().join("downloads/atlas-operations.csv");

    completed_evidence(
        runtime
            .execute(envelope(
                session,
                page,
                PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
                    url,
                    expected_content_type: Some("text/csv".into()),
                    max_bytes: 1024,
                    save_as: Some(destination.to_string_lossy().into_owned()),
                }),
            ))
            .await,
    );

    assert_eq!(
        std::fs::read(destination).unwrap(),
        b"customer,priority\nAtlas Labs,high\n"
    );
    assert_eq!(
        std::fs::read_dir(root.path().join("downloads"))
            .unwrap()
            .count(),
        1
    );
}

#[cfg(unix)]
#[tokio::test]
async fn recovery_publishes_save_as_from_a_durable_prepared_result() {
    use std::os::unix::fs::symlink;

    let url = http_fixture("restart-safe", "application/octet-stream").await;
    let root = tempfile::tempdir().unwrap();
    let journal_path = root.path().join("journal.jsonl");
    let downloads_root = root.path().join("downloads");
    let destination = downloads_root.join("recovered.bin");
    let paused = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let durable = JsonlJournal::open(&journal_path).await.unwrap();
    let journal = Arc::new(DurablePausingJournal {
        inner: durable,
        paused: paused.clone(),
        resume,
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let workers = Arc::new(WorkerPool::new(
        1,
        Arc::new(FakeFactory {
            events: events.clone(),
            mode: DriverMode::Succeed,
            launches: Default::default(),
        }),
    ));
    let network = network_engine::NetworkPolicy {
        allow_loopback: true,
        ..Default::default()
    };
    let store = artifact_store::ArtifactStore::new(root.path(), network.max_download_bytes, 16_384);
    let adaptive = page_runtime::AdaptivePageEngine::new(
        network_engine::EligibilityPolicy::new(network.clone()),
        network_engine::DirectHttpExecutor::new(network.clone()),
        store.clone(),
        network.clone(),
    )
    .with_downloads_root(&downloads_root);
    let runtime = page_runtime::PageRuntime::new_adaptive(journal, workers.clone(), None, adaptive);
    let session = SessionId::new();
    let page = runtime.open_browser(session.clone()).await.unwrap();
    let command = envelope(
        session,
        page.id,
        PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
            url,
            expected_content_type: None,
            max_bytes: 1024,
            save_as: Some(destination.to_string_lossy().into_owned()),
        }),
    );
    let command_id = command.command_id.clone();
    let task = tokio::spawn(async move { runtime.execute(command).await });
    paused.notified().await;
    task.abort();
    let _ = task.await;

    assert!(!destination.exists());
    let durable_json = std::fs::read_to_string(&journal_path).unwrap();
    assert!(!durable_json.contains("recovered.bin"));
    assert!(!durable_json.contains(downloads_root.to_string_lossy().as_ref()));
    let recovered_journal = Arc::new(JsonlJournal::open(&journal_path).await.unwrap());
    let recovered_adaptive = page_runtime::AdaptivePageEngine::new(
        network_engine::EligibilityPolicy::new(network.clone()),
        network_engine::DirectHttpExecutor::new(network.clone()),
        store,
        network,
    )
    .with_downloads_root(&downloads_root);
    let recovered = page_runtime::PageRuntime::new_adaptive(
        recovered_journal,
        workers,
        None,
        recovered_adaptive,
    );

    let staging = std::fs::read_dir(&downloads_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|extension| extension == "tmp"))
        .unwrap();
    let original_staging = downloads_root.join("original-staging");
    std::fs::rename(&staging, &original_staging).unwrap();
    let attacker_file = root.path().join("attacker-controlled");
    std::fs::write(&attacker_file, b"wrong-bytes").unwrap();
    symlink(&attacker_file, &staging).unwrap();
    assert!(matches!(
        recovered.recover_command(command_id.clone()).await,
        CommandOutcome::NeedsReconciliation { .. }
    ));
    assert!(!destination.exists());
    std::fs::remove_file(&staging).unwrap();
    std::fs::rename(&original_staging, &staging).unwrap();

    assert!(matches!(
        recovered.recover_command(command_id.clone()).await,
        CommandOutcome::NeedsReconciliation { .. }
    ));
    assert_eq!(std::fs::read(destination).unwrap(), b"restart-safe");
    let remaining: Vec<_> = std::fs::read_dir(&downloads_root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(remaining, [std::ffi::OsString::from("recovered.bin")]);
    assert!(matches!(
        recovered.recover_command(command_id).await,
        CommandOutcome::NeedsReconciliation { .. }
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn save_as_publication_stays_bound_to_the_validated_directory() {
    use std::os::unix::fs::symlink;

    let url = http_fixture("directory-bound", "application/octet-stream").await;
    let (runtime, session, page, _events, root, paused, resume) =
        adaptive_runtime_paused(CommandPhase::ResultPrepared).await;
    let downloads_root = root.path().join("downloads");
    let original_root = root.path().join("original-downloads");
    let replacement_root = root.path().join("replacement-downloads");
    let destination = downloads_root.join("bound.bin");
    let task = tokio::spawn(async move {
        runtime
            .execute(envelope(
                session,
                page,
                PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
                    url,
                    expected_content_type: None,
                    max_bytes: 1024,
                    save_as: Some(destination.to_string_lossy().into_owned()),
                }),
            ))
            .await
    });
    paused.notified().await;
    std::fs::rename(&downloads_root, &original_root).unwrap();
    std::fs::create_dir(&replacement_root).unwrap();
    symlink(&replacement_root, &downloads_root).unwrap();
    resume.notify_one();

    completed_evidence(task.await.unwrap());
    assert_eq!(
        std::fs::read(original_root.join("bound.bin")).unwrap(),
        b"directory-bound"
    );
    assert!(!replacement_root.join("bound.bin").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn save_as_rejects_a_staging_entry_swapped_before_publication() {
    use std::os::unix::fs::symlink;

    let url = http_fixture("verified-stage", "application/octet-stream").await;
    let (runtime, session, page, _events, root, paused, resume) =
        adaptive_runtime_paused(CommandPhase::ResultPrepared).await;
    let downloads_root = root.path().join("downloads");
    let destination = downloads_root.join("verified.bin");
    let task_destination = destination.clone();
    let task = tokio::spawn(async move {
        runtime
            .execute(envelope(
                session,
                page,
                PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
                    url,
                    expected_content_type: None,
                    max_bytes: 1024,
                    save_as: Some(task_destination.to_string_lossy().into_owned()),
                }),
            ))
            .await
    });
    paused.notified().await;
    let staging = std::fs::read_dir(&downloads_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|extension| extension == "tmp"))
        .unwrap();
    let original_staging = downloads_root.join("original-staging");
    std::fs::rename(&staging, &original_staging).unwrap();
    let attacker_file = root.path().join("attacker-stage");
    std::fs::write(&attacker_file, b"verified-stage").unwrap();
    symlink(&attacker_file, &staging).unwrap();
    resume.notify_one();

    assert!(matches!(
        task.await.unwrap(),
        CommandOutcome::NeedsReconciliation { .. }
    ));
    assert!(!destination.exists());
}

#[tokio::test]
async fn download_url_denies_save_path_outside_downloads_root() {
    let url = http_fixture("private", "text/plain").await;
    let (runtime, session, page, _events, root) = adaptive_runtime(DriverMode::Succeed).await;
    let escaped = root.path().join("escaped.txt");

    let outcome = runtime
        .execute(envelope(
            session,
            page,
            PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
                url,
                expected_content_type: Some("text/plain".into()),
                max_bytes: 1024,
                save_as: Some(escaped.to_string_lossy().into_owned()),
            }),
        ))
        .await;

    assert!(matches!(
        outcome,
        CommandOutcome::PolicyDenied { error, .. } if error.code == ErrorCode::PolicyDenied
    ));
    assert!(!escaped.exists());
}

#[tokio::test]
async fn download_url_denies_invalid_save_path_before_network_fetch() {
    use std::sync::atomic::Ordering;
    let (url, requests) = counted_http_fixture().await;
    let (runtime, session, page, _events, root) = adaptive_runtime(DriverMode::Succeed).await;

    let outcome = runtime
        .execute(envelope(
            session,
            page,
            PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
                url,
                expected_content_type: Some("text/plain".into()),
                max_bytes: 1024,
                save_as: Some(
                    root.path()
                        .join("escaped.txt")
                        .to_string_lossy()
                        .into_owned(),
                ),
            }),
        ))
        .await;

    assert!(matches!(outcome, CommandOutcome::PolicyDenied { .. }));
    assert_eq!(requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn download_url_does_not_overwrite_existing_destination() {
    let url = http_fixture("replacement", "text/plain").await;
    let (runtime, session, page, _events, root) = adaptive_runtime(DriverMode::Succeed).await;
    let destination = root.path().join("downloads/existing.txt");
    std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
    std::fs::write(&destination, b"original").unwrap();

    let outcome = runtime
        .execute(envelope(
            session,
            page,
            PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
                url,
                expected_content_type: Some("text/plain".into()),
                max_bytes: 1024,
                save_as: Some(destination.to_string_lossy().into_owned()),
            }),
        ))
        .await;

    assert!(matches!(
        outcome,
        CommandOutcome::PolicyDenied { error, .. } if error.code == ErrorCode::PolicyDenied
    ));
    assert_eq!(std::fs::read(destination).unwrap(), b"original");
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
                save_as: None,
            }),
        ))
        .await;
    assert!(
        matches!(outcome, CommandOutcome::NeedsReconciliation { error, .. } if error.code == ErrorCode::HttpEquivalenceUnproven)
    );
    let events = events.lock().await;
    assert!(!events.iter().any(|event| event.starts_with("browser:")));
    assert!(!events.contains(&"http:commit".to_string()));
}

#[tokio::test]
async fn download_state_commit_failure_keeps_prepared_artifact_recoverable() {
    let url = http_fixture("guarded-download", "application/octet-stream").await;
    let (runtime, session, page, events, root) = adaptive_runtime(DriverMode::CommitFail).await;
    let destination = root.path().join("downloads/commit-failed.bin");
    events.lock().await.push(format!("url:{url}"));
    let outcome = runtime
        .execute(envelope(
            session.clone(),
            page,
            PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
                url,
                expected_content_type: None,
                max_bytes: 1024,
                save_as: Some(destination.to_string_lossy().into_owned()),
            }),
        ))
        .await;
    assert!(matches!(
        outcome,
        CommandOutcome::NeedsReconciliation { .. }
    ));
    assert_single_download_readable(root.path(), &session, b"guarded-download").await;
    assert_eq!(std::fs::read(destination).unwrap(), b"guarded-download");
    assert_eq!(
        std::fs::read_dir(root.path().join("downloads"))
            .unwrap()
            .count(),
        1
    );
    assert!(
        matches!(outcome, CommandOutcome::NeedsReconciliation { evidence, .. }
        if evidence.iter().any(|item| matches!(item, Evidence::Download { .. })))
    );
}

#[tokio::test]
async fn failed_terminal_journal_append_keeps_save_as_recovery_sidecar() {
    let url = http_fixture("journal-recovery", "application/octet-stream").await;
    let (runtime, session, page, _events, root) =
        adaptive_runtime_with_failure(DriverMode::CommitFail, Some(CommandPhase::Failed)).await;
    let downloads_root = root.path().join("downloads");
    let destination = downloads_root.join("journal-failed.bin");

    let outcome = runtime
        .execute(envelope(
            session,
            page,
            PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
                url,
                expected_content_type: None,
                max_bytes: 1024,
                save_as: Some(destination.to_string_lossy().into_owned()),
            }),
        ))
        .await;

    assert!(matches!(
        outcome,
        CommandOutcome::RetryableFailure { error, .. }
            | CommandOutcome::NeedsReconciliation { error, .. }
            if error.code == ErrorCode::JournalFailed
    ));
    assert_eq!(std::fs::read(destination).unwrap(), b"journal-recovery");
    assert!(std::fs::read_dir(downloads_root).unwrap().any(|entry| {
        entry
            .unwrap()
            .path()
            .extension()
            .is_some_and(|extension| extension == "meta")
    }));
}

#[tokio::test]
async fn download_state_commit_cancellation_keeps_prepared_artifact_recoverable() {
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
                    save_as: None,
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
    assert_single_download_readable(root.path(), &session, b"cancelled-download").await;
}

#[tokio::test]
async fn prepared_result_journal_failure_prevents_state_commit_without_deleting_bytes() {
    let url = http_fixture("prepared-failure", "application/octet-stream").await;
    let (runtime, session, page, events, root) =
        adaptive_runtime_with_failure(DriverMode::Succeed, Some(CommandPhase::ResultPrepared))
            .await;
    let destination = root.path().join("downloads/prepared-failure.bin");
    let outcome = runtime
        .execute(envelope(
            session.clone(),
            page,
            PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
                url,
                expected_content_type: None,
                max_bytes: 1024,
                save_as: Some(destination.to_string_lossy().into_owned()),
            }),
        ))
        .await;
    assert!(matches!(
        outcome,
        CommandOutcome::NeedsReconciliation { .. }
    ));
    assert!(!events.lock().await.contains(&"http:commit".to_string()));
    let directory = root.path().join(session.0.to_string());
    assert!(!directory.exists() || std::fs::read_dir(directory).unwrap().next().is_none());
    assert!(
        !destination.exists(),
        "saveAs must not publish before ResultPrepared is durable"
    );
}

#[tokio::test]
async fn inspect_prepared_append_failure_is_never_replayable() {
    let url = http_fixture("<title>Prepared</title><p>inspect-result</p>", "text/html").await;
    let (runtime, session, page, events, _root) =
        adaptive_runtime_with_failure(DriverMode::Succeed, Some(CommandPhase::ResultPrepared))
            .await;
    runtime
        .set_url(&page, url.clone(), "interactive")
        .await
        .unwrap();
    events.lock().await.push(format!("url:{url}"));
    let outcome = runtime
        .execute(envelope(
            session,
            page,
            PrimitiveCommand::Inspect(InspectCommand::default()),
        ))
        .await;
    assert!(
        matches!(outcome, CommandOutcome::NeedsReconciliation { evidence, .. }
        if evidence.iter().any(|item| matches!(item, Evidence::Inspection { text, .. } if text.contains("inspect-result"))))
    );
    assert!(!events.lock().await.contains(&"http:commit".to_string()));
}

#[tokio::test]
async fn completed_journal_failure_keeps_prepared_state_and_artifact_reconcilable() {
    let url = http_fixture("completion-failure", "application/octet-stream").await;
    let (runtime, session, page, events, root) =
        adaptive_runtime_with_failure(DriverMode::Succeed, Some(CommandPhase::Completed)).await;
    let destination = root.path().join("downloads/completion-failure.bin");
    let outcome = runtime
        .execute(envelope(
            session.clone(),
            page,
            PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
                url,
                expected_content_type: None,
                max_bytes: 1024,
                save_as: Some(destination.to_string_lossy().into_owned()),
            }),
        ))
        .await;
    assert!(matches!(
        outcome,
        CommandOutcome::NeedsReconciliation { .. }
    ));
    assert!(events.lock().await.contains(&"http:commit".to_string()));
    assert_single_download_readable(root.path(), &session, b"completion-failure").await;
    assert_eq!(std::fs::read(destination).unwrap(), b"completion-failure");
}

#[tokio::test]
async fn recovery_never_replays_a_durable_prepared_download() {
    let command_id = CommandId::new();
    let attempt_id = AttemptId::new();
    let evidence = vec![Evidence::Download {
        filename: "x.bin".into(),
        path: "abc".into(),
        bytes: 1,
        sha256: "abc".into(),
    }];
    let journal = Arc::new(RecoveryJournal {
        records: vec![JournalRecord {
            sequence: 0,
            recorded_at: Utc::now(),
            command_id: command_id.clone(),
            phase: CommandPhase::ResultPrepared,
            envelope: None,
            outcome: None,
            prepared_result: Some(PreparedResult {
                command_id: command_id.clone(),
                attempt_id,
                state_version: 1,
                state_delta: serde_json::json!({}),
                evidence: evidence.clone(),
                artifact_id: Some("abc".into()),
                artifact_sha256: Some("abc".into()),
                artifact_bytes: Some(1),
                artifact_staging_id: None,
                download: None,
            }),
        }],
    });
    let workers = Arc::new(WorkerPool::new(
        1,
        Arc::new(FakeFactory {
            events: Arc::new(Mutex::new(Vec::new())),
            mode: DriverMode::Succeed,
            launches: Default::default(),
        }),
    ));
    let runtime = page_runtime::PageRuntime::new(journal, workers);
    let outcome = runtime.recover_command(command_id).await;
    assert!(
        matches!(outcome, CommandOutcome::NeedsReconciliation { evidence: actual, .. } if actual == evidence)
    );
}

#[tokio::test]
async fn recovery_finalizes_a_durable_staged_download_before_reconciliation() {
    let root = tempfile::tempdir().unwrap();
    let store = artifact_store::ArtifactStore::new(root.path(), 1024, 16_384);
    let session = SessionId::new();
    let page = PageId::new();
    let pending = store
        .put_pending(
            &session,
            &page,
            "application/octet-stream",
            "bin",
            b"restart-finalize",
            1024,
        )
        .await
        .unwrap();
    let artifact_id = pending.record().artifact_id.clone();
    let sha256 = pending.record().sha256.clone();
    let staging_id = pending.staging_id().unwrap().to_owned();
    std::mem::forget(pending);

    let command = envelope(
        session.clone(),
        page,
        PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
            url: "https://example.test/recover.bin".into(),
            expected_content_type: None,
            max_bytes: 1024,
            save_as: None,
        }),
    );
    let command_id = command.command_id.clone();
    let prepared = PreparedResult {
        command_id: command_id.clone(),
        attempt_id: command.attempt_id.clone(),
        state_version: 1,
        state_delta: serde_json::Value::Null,
        evidence: vec![Evidence::Download {
            filename: "recover.bin".into(),
            path: artifact_id.clone(),
            bytes: 16,
            sha256: sha256.clone(),
        }],
        artifact_id: Some(artifact_id.clone()),
        artifact_sha256: Some(sha256),
        artifact_bytes: Some(16),
        artifact_staging_id: Some(staging_id),
        download: None,
    };
    let records = vec![
        JournalRecord {
            sequence: 0,
            recorded_at: Utc::now(),
            command_id: command_id.clone(),
            phase: CommandPhase::Accepted,
            envelope: Some(command.journal_safe()),
            outcome: None,
            prepared_result: None,
        },
        JournalRecord {
            sequence: 1,
            recorded_at: Utc::now(),
            command_id: command_id.clone(),
            phase: CommandPhase::ResultPrepared,
            envelope: None,
            outcome: None,
            prepared_result: Some(prepared),
        },
    ];
    let workers = Arc::new(WorkerPool::new(
        1,
        Arc::new(FakeFactory {
            events: Arc::new(Mutex::new(Vec::new())),
            mode: DriverMode::Succeed,
            launches: Default::default(),
        }),
    ));
    let network = network_engine::NetworkPolicy {
        allow_loopback: true,
        ..Default::default()
    };
    let adaptive = page_runtime::AdaptivePageEngine::new(
        network_engine::EligibilityPolicy::new(network.clone()),
        network_engine::DirectHttpExecutor::new(network.clone()),
        store.clone(),
        network,
    );
    let runtime = page_runtime::PageRuntime::new_adaptive(
        Arc::new(RecoveryJournal { records }),
        workers,
        None,
        adaptive,
    );

    assert!(matches!(
        runtime.recover_command(command_id).await,
        CommandOutcome::NeedsReconciliation { .. }
    ));
    assert_eq!(
        store.get(&session, &artifact_id).await.unwrap(),
        b"restart-finalize"
    );
}

#[tokio::test]
async fn cancellation_at_each_post_response_journal_await_preserves_durable_artifact() {
    for phase in [
        CommandPhase::ResultPrepared,
        CommandPhase::Verifying,
        CommandPhase::Completed,
    ] {
        let url = http_fixture("cancel-boundary", "application/octet-stream").await;
        let (runtime, session, page, events, root, paused, resume) =
            adaptive_runtime_paused(phase).await;
        let task_session = session.clone();
        let task = tokio::spawn(async move {
            runtime
                .execute(envelope(
                    task_session,
                    page,
                    PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
                        url,
                        expected_content_type: None,
                        max_bytes: 1024,
                        save_as: None,
                    }),
                ))
                .await
        });
        tokio::time::timeout(StdDuration::from_secs(1), paused.notified())
            .await
            .expect("journal boundary not reached");
        let committed = events.lock().await.contains(&"http:commit".to_string());
        assert_eq!(committed, phase != CommandPhase::ResultPrepared);
        task.abort();
        let _ = task.await;
        if phase == CommandPhase::ResultPrepared {
            resume.notify_one();
            tokio::time::timeout(StdDuration::from_secs(1), async {
                loop {
                    let visible = std::fs::read_dir(root.path().join(session.0.to_string()))
                        .ok()
                        .and_then(|mut entries| entries.find_map(Result::ok))
                        .map(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
                        .unwrap_or(false);
                    if visible {
                        break;
                    }
                    tokio::time::sleep(StdDuration::from_millis(5)).await;
                }
            })
            .await
            .expect("detached durable prepare did not publish artifact");
            assert_single_download_readable(root.path(), &session, b"cancel-boundary").await;
        } else {
            assert_single_download_readable(root.path(), &session, b"cancel-boundary").await;
        }
    }
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
    // Downloads keep the hard terminal denial: the policy boundary exists to
    // stop the runtime's own HTTP client fetching out-of-policy destinations.
    let outcome = runtime
        .execute(envelope(
            session.clone(),
            page.clone(),
            PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
                url: "file:///secret".into(),
                expected_content_type: None,
                max_bytes: 1024,
                save_as: None,
            }),
        ))
        .await;
    assert!(matches!(outcome, CommandOutcome::PolicyDenied { .. }));
    // An inspect of a page the browser already has open is not a fetch: it
    // degrades to the browser instead of failing a DOM read with a network
    // error code. The direct-HTTP path is never consulted either way.
    let outcome = runtime
        .execute(envelope(
            session,
            page,
            PrimitiveCommand::Inspect(InspectCommand::default()),
        ))
        .await;
    assert!(matches!(outcome, CommandOutcome::Completed { .. }));
    let events = events.lock().await;
    assert!(!events.contains(&"http:state".to_string()));
}

#[tokio::test]
async fn a_command_that_outlives_its_deadline_fails_instead_of_hanging() {
    let (runtime, session, page, events) = runtime(DriverMode::SlowInspect, None).await;
    let mut request = envelope(
        session,
        page,
        PrimitiveCommand::Inspect(InspectCommand {
            selector: Some("#slow".into()),
            target: None,
            include_html: false,
        }),
    );
    request.deadline = Utc::now() + Duration::milliseconds(200);

    let outcome = tokio::time::timeout(StdDuration::from_secs(2), runtime.execute(request))
        .await
        .expect("execute() did not return promptly; envelope deadline may not be enforced");

    assert!(events.lock().await.contains(&"browser:inspect".to_string()));

    let error = match outcome {
        CommandOutcome::Failed { error, .. } | CommandOutcome::RetryableFailure { error, .. } => {
            error
        }
        other => panic!("hung command must fail at its deadline: {other:?}"),
    };
    assert_eq!(error.code, ErrorCode::DeadlineExceeded, "{error:?}");
}

#[tokio::test]
async fn a_typed_value_that_never_lands_fails_verification() {
    // The positive case above only proves the happy path because the fake
    // echoes the typed value back. This one returns a different page state:
    // the runtime's post-type verification must fail the command, or the
    // whole "verify every effect" contract is decorative.
    let (runtime, session, page, _) = runtime(DriverMode::InspectMismatch, None).await;
    let outcome = runtime
        .execute(envelope(
            session,
            page,
            PrimitiveCommand::TypeText(TypeTextCommand {
                selector: "input[name='email']".into(),
                target: None,
                value: "ada@example.test".into(),
                clear_first: true,
                expected_url: None,
            }),
        ))
        .await;
    let error = match outcome {
        CommandOutcome::Failed { error, .. } | CommandOutcome::RetryableFailure { error, .. } => {
            error
        }
        other => panic!("mismatched page state must fail verification: {other:?}"),
    };
    assert_eq!(error.code, ErrorCode::VerificationFailed, "{error:?}");
}
