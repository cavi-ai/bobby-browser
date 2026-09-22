use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use types::{
    ClickCommand, CommandError, Evidence, InspectCommand, NavigateCommand, PageId, SessionId,
    TypeTextCommand, WorkerId,
};
use worker_pool::{BrowserWorker, WorkerFactory, WorkerPool, DEFAULT_REPLACEMENT_CLEANUP_TIMEOUT};

struct HangingFactory {
    launches: Arc<AtomicUsize>,
}

#[async_trait]
impl WorkerFactory for HangingFactory {
    async fn launch(
        &self,
        _session_id: &SessionId,
    ) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        // Never completes: proves the outer lease deadline fails closed.
        std::future::pending::<()>().await;
        unreachable!()
    }
}

struct FastFailFactory;

#[async_trait]
impl WorkerFactory for FastFailFactory {
    async fn launch(
        &self,
        _session_id: &SessionId,
    ) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        Err(CommandError {
            code: types::ErrorCode::BrowserLaunchFailed,
            message: "companion bind refused on 127.0.0.1:0".into(),
            layer: types::ErrorLayer::Driver,
            retryable: true,
        })
    }
}

struct ReadyWorker {
    id: WorkerId,
}

#[async_trait]
impl BrowserWorker for ReadyWorker {
    fn worker_id(&self) -> WorkerId {
        self.id.clone()
    }
    fn profile_dir(&self) -> &Path {
        Path::new("/profiles/ready")
    }
    async fn open_page(&self, _: PageId) -> Result<(), CommandError> {
        Ok(())
    }
    async fn navigate(
        &self,
        _: &PageId,
        _: &NavigateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![])
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

struct ReadyFactory;

#[async_trait]
impl WorkerFactory for ReadyFactory {
    async fn launch(
        &self,
        _session_id: &SessionId,
    ) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        Ok(Arc::new(ReadyWorker {
            id: WorkerId::new(),
        }))
    }
}

#[tokio::test]
async fn lease_fails_closed_when_factory_launch_hangs_past_outer_deadline() {
    let launches = Arc::new(AtomicUsize::new(0));
    let pool = WorkerPool::with_timeouts(
        2,
        Arc::new(HangingFactory {
            launches: launches.clone(),
        }),
        DEFAULT_REPLACEMENT_CLEANUP_TIMEOUT,
        Duration::from_millis(80),
    );

    let started = std::time::Instant::now();
    let error = pool
        .lease(SessionId::new())
        .await
        .err()
        .expect("hung factory must fail the lease");
    let elapsed = started.elapsed();

    assert_eq!(error.code, types::ErrorCode::BrowserLaunchFailed);
    assert!(
        error.message.contains("outer deadline"),
        "expected outer deadline diagnostic, got {}",
        error.message
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "outer deadline should trip well under host MCP empty-timeout (~80s); elapsed={elapsed:?}"
    );
    assert_eq!(launches.load(Ordering::SeqCst), 1);
    assert_eq!(pool.active_workers().await, 0);
}

#[tokio::test]
async fn lease_preserves_concrete_factory_launch_error() {
    let pool = WorkerPool::new(2, Arc::new(FastFailFactory));
    let error = pool
        .lease(SessionId::new())
        .await
        .err()
        .expect("fast-fail factory must surface its error");
    assert_eq!(error.code, types::ErrorCode::BrowserLaunchFailed);
    assert!(
        error.message.contains("companion bind refused"),
        "concrete factory error must be preserved, got {}",
        error.message
    );
    assert_eq!(pool.active_workers().await, 0);
}

#[tokio::test]
async fn lease_still_succeeds_for_ready_factory_under_deadline() {
    let pool = WorkerPool::with_timeouts(
        2,
        Arc::new(ReadyFactory),
        DEFAULT_REPLACEMENT_CLEANUP_TIMEOUT,
        Duration::from_secs(2),
    );
    let lease = pool.lease(SessionId::new()).await.expect("ready factory");
    assert_eq!(pool.active_workers().await, 1);
    drop(lease);
}

struct ClosingWorker {
    id: WorkerId,
    closed: Arc<AtomicBool>,
}

#[async_trait]
impl BrowserWorker for ClosingWorker {
    fn worker_id(&self) -> WorkerId {
        self.id.clone()
    }
    fn profile_dir(&self) -> &Path {
        Path::new("/profiles/closing")
    }
    async fn open_page(&self, _: PageId) -> Result<(), CommandError> {
        Ok(())
    }
    async fn navigate(
        &self,
        _: &PageId,
        _: &NavigateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![])
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
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// First launch sleeps `first_delay` (or hangs when `None`); later launches
/// sleep `later_delay`. Counts launches and factory session releases.
struct ScriptedFactory {
    first_delay: Option<Duration>,
    later_delay: Duration,
    launches: AtomicUsize,
    releases: Arc<AtomicUsize>,
    closed: Arc<AtomicBool>,
}

impl ScriptedFactory {
    fn new(first_delay: Option<Duration>) -> Self {
        Self {
            first_delay,
            later_delay: Duration::ZERO,
            launches: AtomicUsize::new(0),
            releases: Arc::new(AtomicUsize::new(0)),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[async_trait]
impl WorkerFactory for ScriptedFactory {
    async fn launch(&self, _: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        if self.launches.fetch_add(1, Ordering::SeqCst) == 0 {
            match self.first_delay {
                Some(delay) => tokio::time::sleep(delay).await,
                None => std::future::pending::<()>().await,
            }
        } else {
            tokio::time::sleep(self.later_delay).await;
        }
        Ok(Arc::new(ClosingWorker {
            id: WorkerId::new(),
            closed: self.closed.clone(),
        }))
    }

    async fn release_session(&self, _: &SessionId) {
        self.releases.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn a_launch_finishing_after_the_deadline_terminates_its_worker() {
    let factory = Arc::new(ScriptedFactory::new(Some(Duration::from_millis(150))));
    let pool = WorkerPool::with_timeouts(
        2,
        factory.clone(),
        DEFAULT_REPLACEMENT_CLEANUP_TIMEOUT,
        Duration::from_millis(80),
    );
    let error = pool.lease(SessionId::new()).await.err().expect("deadline");
    assert!(
        error.message.contains("outer deadline"),
        "{}",
        error.message
    );
    assert!(!factory.closed.load(Ordering::SeqCst));

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        factory.closed.load(Ordering::SeqCst),
        "late worker must be terminated, not leaked"
    );
    assert_eq!(factory.releases.load(Ordering::SeqCst), 1);
    assert_eq!(pool.active_workers().await, 0);
}

#[tokio::test]
async fn a_launch_hung_past_the_grace_deadline_is_reaped() {
    let factory = Arc::new(ScriptedFactory::new(None));
    let pool = WorkerPool::with_timeouts(
        2,
        factory.clone(),
        DEFAULT_REPLACEMENT_CLEANUP_TIMEOUT,
        Duration::from_millis(80),
    );
    let session = SessionId::new();
    assert!(pool.lease(session.clone()).await.is_err());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(factory.releases.load(Ordering::SeqCst), 1);

    // The scrubbed entry lets the next lease launch fresh.
    let lease = pool.lease(session).await.expect("relaunch after reap");
    assert_eq!(factory.launches.load(Ordering::SeqCst), 2);
    drop(lease);
}

/// A second lease that takes over the hung entry's initialization must keep
/// its worker: the reaper waits for it and then finds the entry initialized.
#[tokio::test]
async fn the_reaper_leaves_an_entry_a_concurrent_lease_initialized() {
    let factory = Arc::new(ScriptedFactory {
        later_delay: Duration::from_millis(50),
        ..ScriptedFactory::new(None)
    });
    let pool = WorkerPool::with_timeouts(
        2,
        factory.clone(),
        DEFAULT_REPLACEMENT_CLEANUP_TIMEOUT,
        Duration::from_millis(200),
    );
    let session = SessionId::new();
    let first = tokio::spawn({
        let pool = pool.clone();
        let session = session.clone();
        async move { pool.lease(session).await.err() }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let lease = pool
        .lease(session.clone())
        .await
        .expect("takeover lease after the hung launch is aborted");
    assert!(first.await.unwrap().is_some());
    assert_eq!(factory.launches.load(Ordering::SeqCst), 2);
    drop(lease);

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(pool.active_workers().await, 1);
    assert_eq!(factory.releases.load(Ordering::SeqCst), 0);
    assert!(!factory.closed.load(Ordering::SeqCst));
}
