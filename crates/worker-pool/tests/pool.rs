use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use types::{
    ClickCommand, CommandError, Evidence, InspectCommand, NavigateCommand, PageId, SessionId,
    TypeTextCommand, WorkerId,
};
use worker_pool::{BrowserWorker, WorkerFactory, WorkerPool};

struct FakeWorker {
    id: WorkerId,
    profile: PathBuf,
    terminations: Arc<AtomicUsize>,
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
    async fn terminate(&self) -> Result<(), CommandError> {
        self.terminations.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Default)]
struct FakeFactory {
    launches: AtomicUsize,
    terminations: Arc<AtomicUsize>,
}

struct FailOnceFactory {
    attempts: AtomicUsize,
}

#[async_trait]
impl WorkerFactory for FailOnceFactory {
    async fn launch(&self, session_id: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(CommandError {
                code: types::ErrorCode::BrowserLaunchFailed,
                message: "injected launch failure".into(),
                layer: types::ErrorLayer::Driver,
                retryable: true,
            });
        }
        Ok(Arc::new(FakeWorker {
            id: WorkerId::new(),
            profile: PathBuf::from(format!("/profiles/{}", session_id.0)),
            terminations: Arc::default(),
        }))
    }
}

#[async_trait]
impl WorkerFactory for FakeFactory {
    async fn launch(&self, session_id: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(FakeWorker {
            id: WorkerId::new(),
            profile: PathBuf::from(format!("/profiles/{}", session_id.0)),
            terminations: self.terminations.clone(),
        }))
    }
}

#[tokio::test]
async fn reuses_one_dedicated_worker_per_session() {
    let factory = Arc::new(FakeFactory::default());
    let pool = WorkerPool::new(8, factory.clone());
    let session = SessionId::new();
    let first = pool.lease(session.clone()).await.unwrap();
    let second = pool.lease(session).await.unwrap();
    assert_eq!(first.worker_id(), second.worker_id());
    assert_eq!(factory.launches.load(Ordering::SeqCst), 1);
    assert_eq!(pool.active_workers().await, 1);
}

#[tokio::test]
async fn thirty_two_warm_workers_preserve_identity_while_only_eight_leases_are_active() {
    let pool = WorkerPool::new(8, Arc::new(FakeFactory::default()));
    let mut sessions = Vec::new();
    let mut identities = Vec::new();
    for _ in 0..32 {
        let session = SessionId::new();
        let lease = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            pool.lease(session.clone()),
        )
        .await
        .expect("warm session creation must not retain an active-work permit")
        .unwrap();
        identities.push(lease.worker_id());
        drop(lease);
        sessions.push(session);
    }
    assert_eq!(pool.active_workers().await, 32);

    let mut active = Vec::new();
    for session in sessions.iter().take(8) {
        active.push(pool.lease(session.clone()).await.unwrap());
    }
    let ninth = SessionId::new();
    let pending_pool = pool.clone();
    let task = tokio::spawn(async move { pending_pool.lease(ninth).await.unwrap() });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), async {
            while !task.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_err()
    );

    drop(active.remove(0));
    let ninth = tokio::time::timeout(std::time::Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
    drop(ninth);

    let resumed = pool.lease(sessions[0].clone()).await.unwrap();
    assert_eq!(resumed.worker_id(), identities[0]);
}

#[tokio::test]
async fn cancelling_a_waiting_lease_does_not_leak_capacity() {
    let pool = WorkerPool::new(1, Arc::new(FakeFactory::default()));
    let held = pool.lease(SessionId::new()).await.unwrap();
    let pending_pool = pool.clone();
    let pending = tokio::spawn(async move { pending_pool.lease(SessionId::new()).await });
    tokio::task::yield_now().await;
    pending.abort();
    let _ = pending.await;
    drop(held);
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        pool.lease(SessionId::new()),
    )
    .await
    .expect("cancelled waiter leaked the active-work permit")
    .unwrap();
}

#[tokio::test]
async fn distinct_sessions_receive_distinct_workers_and_profiles() {
    let pool = WorkerPool::new(8, Arc::new(FakeFactory::default()));
    let first = pool.lease(SessionId::new()).await.unwrap();
    let second = pool.lease(SessionId::new()).await.unwrap();
    assert_ne!(first.worker_id(), second.worker_id());
    assert_ne!(first.profile_dir(), second.profile_dir());
}

#[tokio::test]
async fn launch_failure_releases_capacity_for_retry() {
    let pool = WorkerPool::new(
        1,
        Arc::new(FailOnceFactory {
            attempts: AtomicUsize::new(0),
        }),
    );
    let session = SessionId::new();
    assert!(pool.lease(session.clone()).await.is_err());
    tokio::time::timeout(std::time::Duration::from_secs(1), pool.lease(session))
        .await
        .expect("retry should not wait on a leaked permit")
        .unwrap();
}

#[tokio::test]
async fn invalidate_terminates_and_replaces_worker_without_changing_profile() {
    let factory = Arc::new(FakeFactory::default());
    let pool = WorkerPool::new(1, factory.clone());
    let session = SessionId::new();
    let first = pool.lease(session.clone()).await.unwrap();
    let first_id = first.worker_id();
    let profile = first.profile_dir().to_path_buf();

    pool.invalidate_session(&session).await.unwrap();
    assert_eq!(factory.terminations.load(Ordering::SeqCst), 1);
    assert_eq!(pool.active_workers().await, 0);
    drop(first);

    let replacement = pool.lease(session.clone()).await.unwrap();
    assert_ne!(replacement.worker_id(), first_id);
    assert_eq!(replacement.profile_dir(), profile);
    assert_eq!(pool.active_workers().await, 1);

    pool.invalidate_session(&SessionId::new()).await.unwrap();
    assert_eq!(factory.terminations.load(Ordering::SeqCst), 1);
}
