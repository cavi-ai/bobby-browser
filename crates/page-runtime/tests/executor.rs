use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use tokio::sync::Mutex;
use types::{
    AttemptId, ClickCommand, CommandEnvelope, CommandError, CommandId, CommandOutcome,
    CommandPhase, ErrorCode, ErrorLayer, Evidence, InspectCommand, NavigateCommand, PageId,
    PrimitiveCommand, SessionId, TypeTextCommand, WaitUntil, WorkerId, WorkflowId,
};
use worker_pool::{BrowserWorker, WorkerFactory, WorkerPool};
use workflow_journal::{CommandJournal, JournalError, JournalRecord, JournalScan};

#[derive(Clone, Copy)]
enum DriverMode {
    Succeed,
    FailInspect,
    FailClick,
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
async fn boundary_prepare_failure_is_safe_to_retry() {
    let (runtime, session, page, events) =
        runtime(DriverMode::Succeed, Some(CommandPhase::Prepared)).await;
    let outcome = runtime
        .execute(envelope(
            session,
            page,
            PrimitiveCommand::Click(ClickCommand {
                selector: "#submit".into(),
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
