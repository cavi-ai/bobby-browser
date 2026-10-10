use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use interface_core::{AuthorityStore, RuntimeInterface};
use page_runtime::PageRuntime;
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use session_manager::SessionManager;
use types::{
    AccessibilitySnapshotCommand, AttemptId, Capability, ClickCommand, CommandEnvelope,
    CommandError, CommandId, CommandOutcome, CreateSessionRequest, ErrorCode, ErrorLayer, Evidence,
    InspectCommand, InterfaceErrorCode, NavigateCommand, OpenPageRequest, PageId, PrimitiveCommand,
    PrincipalId, RequestContext, RuntimeCommand, SessionId, TypeTextCommand, WorkerId, WorkflowId,
};
use worker_pool::{BrowserWorker, WorkerFactory, WorkerPool};
use workflow_journal::JsonlJournal;

#[derive(Clone, Copy)]
enum Death {
    Gone,
    HungClose,
}

struct DeadableWorker {
    id: WorkerId,
    profile: PathBuf,
    dead: Arc<AtomicBool>,
    death: Death,
}

impl DeadableWorker {
    fn check(&self) -> Result<(), CommandError> {
        if self.dead.load(Ordering::SeqCst) {
            return Err(CommandError {
                code: ErrorCode::BrowserCommandFailed,
                message: worker_pool::FIREFOX_WORKER_CLOSED_MESSAGE.into(),
                layer: ErrorLayer::Driver,
                retryable: false,
            });
        }
        Ok(())
    }
}

#[async_trait]
impl BrowserWorker for DeadableWorker {
    fn worker_id(&self) -> WorkerId {
        self.id.clone()
    }

    fn profile_dir(&self) -> &Path {
        &self.profile
    }

    async fn close(&self) -> Result<(), CommandError> {
        if !self.dead.load(Ordering::SeqCst) {
            return Ok(());
        }
        match self.death {
            Death::Gone => Err(CommandError {
                code: ErrorCode::BrowserCommandFailed,
                message: "Firefox lifecycle cleanup failed while closing the Firefox transport: \
                          Firefox BiDi transport closed"
                    .into(),
                layer: ErrorLayer::Driver,
                retryable: false,
            }),
            Death::HungClose => std::future::pending().await,
        }
    }

    fn tabs(&self) -> Option<&dyn worker_pool::TabsEngine> {
        Some(self)
    }

    fn navigation(&self) -> Option<&dyn worker_pool::NavigationEngine> {
        Some(self)
    }

    fn observation(&self) -> Option<&dyn worker_pool::ObservationEngine> {
        Some(self)
    }

    fn input(&self) -> Option<&dyn worker_pool::InputEngine> {
        Some(self)
    }
}

#[async_trait]
impl worker_pool::TabsEngine for DeadableWorker {
    async fn open_page(&self, _: PageId) -> Result<(), CommandError> {
        self.check()
    }
}

#[async_trait]
impl worker_pool::NavigationEngine for DeadableWorker {
    async fn navigate(
        &self,
        _: &PageId,
        _: &NavigateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.check().map(|()| Vec::new())
    }
}

#[async_trait]
impl worker_pool::ObservationEngine for DeadableWorker {
    async fn inspect(&self, _: &PageId, _: &InspectCommand) -> Result<Vec<Evidence>, CommandError> {
        self.check().map(|()| Vec::new())
    }

    async fn a11y_snapshot(
        &self,
        _: &PageId,
        _: &AccessibilitySnapshotCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.check().map(|()| Vec::new())
    }

    async fn form_snapshot(
        &self,
        _: &PageId,
        _: Option<u32>,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.check().map(|()| Vec::new())
    }
}

#[async_trait]
impl worker_pool::InputEngine for DeadableWorker {
    async fn click(&self, _: &PageId, _: &ClickCommand) -> Result<Vec<Evidence>, CommandError> {
        self.check().map(|()| Vec::new())
    }

    async fn type_text(
        &self,
        _: &PageId,
        _: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.check().map(|()| Vec::new())
    }
}

struct DeadableFactory {
    dead: Arc<AtomicBool>,
    death: Death,
}

#[async_trait]
impl WorkerFactory for DeadableFactory {
    async fn launch(&self, session: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        Ok(Arc::new(DeadableWorker {
            id: WorkerId::new(),
            profile: PathBuf::from(format!("/dead/{}", session.0)),
            dead: self.dead.clone(),
            death: self.death,
        }))
    }
}

struct Rig {
    api: AuthenticatedRuntime,
    context: RequestContext,
    dead: Arc<AtomicBool>,
    session: SessionId,
    page: PageId,
    _root: tempfile::TempDir,
}

async fn rig(death: Death) -> Rig {
    let root = tempfile::tempdir().unwrap();
    let dead = Arc::new(AtomicBool::new(false));
    let workers = Arc::new(WorkerPool::new(
        2,
        Arc::new(DeadableFactory {
            dead: dead.clone(),
            death,
        }),
    ));
    let manager = SessionManager::new(workers.clone());
    let manager = manager.with_release_timeout(Duration::from_millis(200));
    let runtime = RuntimeService::new(
        manager,
        PageRuntime::new(
            Arc::new(
                JsonlJournal::open(root.path().join("commands.jsonl"))
                    .await
                    .unwrap(),
            ),
            workers,
        ),
    );
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::uuid!("10000000-0000-0000-0000-0000000000d1")),
            [
                Capability::SessionRead,
                Capability::SessionWrite,
                Capability::PageRead,
                Capability::PageWrite,
                Capability::BrowserMutate,
            ],
            Utc::now() + chrono::Duration::minutes(10),
        )
        .await
        .unwrap()
        .expose_once();
    let handle = authority.verify(&token).await.unwrap();
    let api = AuthenticatedRuntime::new(runtime, handle.clone());
    let context = handle.context(Utc::now() + chrono::Duration::minutes(5), None);
    let session = api
        .create_session(
            context.clone(),
            CreateSessionRequest {
                profile: "dead".into(),
                proxy: None,
                execution_policy: Default::default(),
                zigzagzig: false,
            },
        )
        .await
        .unwrap();
    let page = api
        .open_page(
            context.clone(),
            OpenPageRequest {
                session_id: session.id.clone(),
            },
        )
        .await
        .unwrap();
    Rig {
        api,
        context,
        dead,
        session: session.id,
        page: page.id,
        _root: root,
    }
}

fn observe_envelope(rig: &Rig) -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: rig.session.clone(),
        page_id: Some(rig.page.clone()),
        deadline: Utc::now() + chrono::Duration::seconds(30),
        command: RuntimeCommand::Primitive(PrimitiveCommand::AccessibilitySnapshot(
            AccessibilitySnapshotCommand {
                max_nodes: Some(32),
                target: None,
            },
        )),
    }
}

fn failure_of(outcome: &CommandOutcome) -> &CommandError {
    match outcome {
        CommandOutcome::Failed { error, .. } | CommandOutcome::RetryableFailure { error, .. } => {
            error
        }
        other => panic!("expected a failure outcome, got {other:?}"),
    }
}

#[tokio::test]
async fn observe_on_a_dead_browser_reports_the_browser_gone_not_internal() {
    let rig = rig(Death::Gone).await;
    rig.dead.store(true, Ordering::SeqCst);

    let outcome = rig
        .api
        .submit(rig.context.clone(), observe_envelope(&rig))
        .await
        .expect("a dead browser is a command outcome, not an interface fault");
    let error = failure_of(&outcome);
    assert_eq!(error.code, ErrorCode::BrowserCommandFailed, "{error:?}");
    assert!(error.message.contains("create a new session"), "{error:?}");

    let forms = rig
        .api
        .form_snapshot(
            rig.context.clone(),
            rig.session.clone(),
            rig.page.clone(),
            None,
        )
        .await
        .expect_err("form snapshot cannot succeed on a dead browser");
    assert_eq!(
        forms.code,
        InterfaceErrorCode::EngineUnreachable,
        "{forms:?}"
    );
    assert!(forms.message.contains("create a new session"), "{forms:?}");
}

#[tokio::test]
async fn session_close_removes_a_session_whose_browser_teardown_fails() {
    let rig = rig(Death::Gone).await;
    rig.dead.store(true, Ordering::SeqCst);

    rig.api
        .delete_session(rig.context.clone(), rig.session.clone())
        .await
        .expect("close succeeds although the browser cannot be torn down");
    let listed = rig.api.list_sessions(rig.context.clone()).await.unwrap();
    assert!(listed.is_empty(), "{listed:?}");
    rig.api
        .delete_session(rig.context.clone(), rig.session.clone())
        .await
        .expect_err("a second close finds nothing to close");
}

#[tokio::test]
async fn session_close_is_bounded_when_the_browser_hangs() {
    let rig = rig(Death::HungClose).await;
    rig.dead.store(true, Ordering::SeqCst);

    tokio::time::timeout(
        Duration::from_secs(5),
        rig.api
            .delete_session(rig.context.clone(), rig.session.clone()),
    )
    .await
    .expect("close is bounded")
    .expect("close succeeds although the browser hangs");
    let listed = rig.api.list_sessions(rig.context.clone()).await.unwrap();
    assert!(listed.is_empty(), "{listed:?}");
}
