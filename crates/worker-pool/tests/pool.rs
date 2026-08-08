use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use types::{
    ClickCommand, CommandError, Evidence, InspectCommand, NavigateCommand, PageId, SessionId,
    TypeTextCommand, WorkerId,
};
use worker_pool::{BrowserWorker, WorkerFactory, WorkerPool};

struct BlockingCleanupFactory {
    close_started: Arc<tokio::sync::Notify>,
    finish_close: Arc<tokio::sync::Notify>,
    releases: Arc<AtomicUsize>,
}

struct BlockingCleanupWorker {
    close_started: Arc<tokio::sync::Notify>,
    finish_close: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl WorkerFactory for BlockingCleanupFactory {
    async fn launch(
        &self,
        _session_id: &SessionId,
    ) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        Ok(Arc::new(BlockingCleanupWorker {
            close_started: self.close_started.clone(),
            finish_close: self.finish_close.clone(),
        }))
    }

    async fn release_session(&self, _session_id: &SessionId) {
        self.releases.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl BrowserWorker for BlockingCleanupWorker {
    fn worker_id(&self) -> WorkerId {
        WorkerId::new()
    }
    fn profile_dir(&self) -> &Path {
        Path::new("blocking-cleanup")
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
        self.close_started.notify_one();
        self.finish_close.notified().await;
        Ok(())
    }
    async fn terminate(&self) -> Result<(), CommandError> {
        self.close().await
    }
}

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

struct RecoveringFactory {
    attempts: AtomicUsize,
    releases: AtomicUsize,
}

struct BlockingLaunchFactory {
    started: Arc<tokio::sync::Notify>,
    finish: Arc<tokio::sync::Notify>,
    releases: Arc<AtomicUsize>,
    launches: Arc<AtomicUsize>,
    second_started: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl WorkerFactory for BlockingLaunchFactory {
    async fn launch(&self, session_id: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        let attempt = self.launches.fetch_add(1, Ordering::SeqCst);
        if attempt == 0 {
            self.started.notify_one();
            self.finish.notified().await;
        } else {
            self.second_started.notify_one();
        }
        Ok(Arc::new(FakeWorker {
            id: WorkerId::new(),
            profile: PathBuf::from(format!("/profiles/{}", session_id.0)),
            terminations: Arc::default(),
        }))
    }

    async fn release_session(&self, _session_id: &SessionId) {
        self.releases.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl WorkerFactory for RecoveringFactory {
    async fn launch(&self, session_id: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(CommandError {
                code: types::ErrorCode::BrowserLaunchFailed,
                message: "first fresh session fails".into(),
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

    async fn release_session(&self, _session_id: &SessionId) {
        self.releases.fetch_add(1, Ordering::SeqCst);
    }
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
async fn launch_failure_releases_factory_state_before_a_different_fresh_session() {
    let factory = Arc::new(RecoveringFactory {
        attempts: AtomicUsize::new(0),
        releases: AtomicUsize::new(0),
    });
    let pool = WorkerPool::new(1, factory.clone());
    let failed = SessionId::new();
    assert!(pool.lease(failed).await.is_err());
    assert_eq!(factory.releases.load(Ordering::SeqCst), 1);

    let fresh = SessionId::new();
    let lease = pool.lease(fresh.clone()).await.unwrap();
    assert_eq!(
        lease.profile_dir(),
        PathBuf::from(format!("/profiles/{}", fresh.0))
    );
}

#[tokio::test]
async fn aborting_mid_launch_rolls_back_entry_and_factory_selection() {
    let started = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    let releases = Arc::new(AtomicUsize::new(0));
    let launches = Arc::new(AtomicUsize::new(0));
    let second_started = Arc::new(tokio::sync::Notify::new());
    let pool = WorkerPool::new(
        1,
        Arc::new(BlockingLaunchFactory {
            started: started.clone(),
            finish: finish.clone(),
            releases: releases.clone(),
            launches: launches.clone(),
            second_started,
        }),
    );
    let abandoned_session = SessionId::new();
    let pending_pool = pool.clone();
    let pending = tokio::spawn(async move { pending_pool.lease(abandoned_session).await });
    started.notified().await;
    pending.abort();
    let _ = pending.await;
    finish.notify_waiters();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while releases.load(Ordering::SeqCst) == 0 || pool.active_workers().await != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("abandoned launch retained its pool entry or factory selection");

    let fresh = SessionId::new();
    let lease = pool.lease(fresh.clone()).await.unwrap();
    assert_eq!(
        lease.profile_dir(),
        PathBuf::from(format!("/profiles/{}", fresh.0))
    );
    assert_eq!(launches.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn abandoned_launch_retains_capacity_until_owned_rollback_finishes() {
    let started = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    let second_started = Arc::new(tokio::sync::Notify::new());
    let pool = WorkerPool::new(
        1,
        Arc::new(BlockingLaunchFactory {
            started: started.clone(),
            finish: finish.clone(),
            releases: Arc::new(AtomicUsize::new(0)),
            launches: Arc::new(AtomicUsize::new(0)),
            second_started: second_started.clone(),
        }),
    );
    let first_pool = pool.clone();
    let first = tokio::spawn(async move { first_pool.lease(SessionId::new()).await });
    started.notified().await;
    first.abort();
    let _ = first.await;

    let second_pool = pool.clone();
    let second = tokio::spawn(async move { second_pool.lease(SessionId::new()).await });
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            second_started.notified()
        )
        .await
        .is_err(),
        "second launch started while abandoned launch still owned capacity"
    );
    finish.notify_waiters();
    tokio::time::timeout(std::time::Duration::from_secs(1), second_started.notified())
        .await
        .expect("second launch did not begin after rollback released capacity");
    second.await.unwrap().unwrap();
}

#[tokio::test]
async fn invalidate_terminates_and_replaces_worker_without_changing_profile() {
    let factory = Arc::new(FakeFactory::default());
    let pool = WorkerPool::new(1, factory.clone());
    let session = SessionId::new();
    let first = pool.lease(session.clone()).await.unwrap();
    let first_id = first.worker_id();
    let profile = first.profile_dir().to_path_buf();
    drop(first);

    pool.invalidate_session(&session).await.unwrap();
    assert_eq!(factory.terminations.load(Ordering::SeqCst), 1);
    assert_eq!(pool.active_workers().await, 0);

    let replacement = pool.lease(session.clone()).await.unwrap();
    assert_ne!(replacement.worker_id(), first_id);
    assert_eq!(replacement.profile_dir(), profile);
    assert_eq!(pool.active_workers().await, 1);

    pool.invalidate_session(&SessionId::new()).await.unwrap();
    assert_eq!(factory.terminations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stale_worker_invalidation_does_not_remove_its_replacement() {
    let factory = Arc::new(FakeFactory::default());
    let pool = WorkerPool::new(1, factory.clone());
    let session = SessionId::new();
    let failed = pool.lease(session.clone()).await.unwrap();
    let failed_id = failed.worker_id();
    drop(failed);

    pool.invalidate_session_if_worker(&session, &failed_id)
        .await
        .unwrap();
    let replacement = pool.lease(session.clone()).await.unwrap();
    let replacement_id = replacement.worker_id();
    drop(replacement);

    pool.invalidate_session_if_worker(&session, &failed_id)
        .await
        .unwrap();
    let current = pool.lease(session).await.unwrap();

    assert_eq!(current.worker_id(), replacement_id);
    assert_eq!(factory.terminations.load(Ordering::SeqCst), 1);
}

async fn cancellation_does_not_stop_cleanup(invalidate: bool) {
    let close_started = Arc::new(tokio::sync::Notify::new());
    let finish_close = Arc::new(tokio::sync::Notify::new());
    let releases = Arc::new(AtomicUsize::new(0));
    let pool = WorkerPool::new(
        1,
        Arc::new(BlockingCleanupFactory {
            close_started: close_started.clone(),
            finish_close: finish_close.clone(),
            releases: releases.clone(),
        }),
    );
    let session = SessionId::new();
    drop(pool.lease(session.clone()).await.unwrap());
    let cleanup_pool = pool.clone();
    let cleanup_session = session.clone();
    let task = tokio::spawn(async move {
        if invalidate {
            cleanup_pool.invalidate_session(&cleanup_session).await
        } else {
            cleanup_pool.release_session(&cleanup_session).await
        }
    });
    close_started.notified().await;
    task.abort();
    let _ = task.await;
    finish_close.notify_waiters();

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while releases.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled caller stopped owned cleanup");
    assert_eq!(pool.active_workers().await, 0);
}

#[tokio::test]
async fn cancelled_release_session_still_finishes_factory_cleanup() {
    cancellation_does_not_stop_cleanup(false).await;
}

#[tokio::test]
async fn release_waits_for_the_last_shared_lease_before_cleanup() {
    let close_started = Arc::new(tokio::sync::Notify::new());
    let finish_close = Arc::new(tokio::sync::Notify::new());
    let pool = WorkerPool::new(
        1,
        Arc::new(BlockingCleanupFactory {
            close_started: close_started.clone(),
            finish_close: finish_close.clone(),
            releases: Arc::new(AtomicUsize::new(0)),
        }),
    );
    let session = SessionId::new();
    let lease = pool.lease(session.clone()).await.unwrap();
    let release_pool = pool.clone();
    let release_session = session.clone();
    let release = tokio::spawn(async move { release_pool.release_session(&release_session).await });

    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(20),
        close_started.notified()
    )
    .await
    .is_err());

    drop(lease);
    tokio::time::timeout(std::time::Duration::from_secs(1), close_started.notified())
        .await
        .expect("release did not begin after the last lease dropped");
    finish_close.notify_waiters();
    release.await.unwrap().unwrap();
}

#[tokio::test]
async fn cancelled_invalidate_session_still_finishes_factory_cleanup() {
    cancellation_does_not_stop_cleanup(true).await;
}
