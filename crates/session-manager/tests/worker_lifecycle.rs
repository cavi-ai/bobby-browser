use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use session_manager::SessionManager;
use types::{
    ClickCommand, CommandError, CreateSessionRequest, Evidence, InspectCommand, NavigateCommand,
    PageId, SessionId, TypeTextCommand, WorkerId,
};
use worker_pool::{BrowserWorker, WorkerFactory, WorkerPool};

struct LifecycleWorker {
    id: WorkerId,
    profile: PathBuf,
    closed: Arc<AtomicBool>,
}

#[async_trait]
impl BrowserWorker for LifecycleWorker {
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
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

struct LifecycleFactory {
    closed: Arc<AtomicBool>,
}

#[async_trait]
impl WorkerFactory for LifecycleFactory {
    async fn launch(&self, session_id: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        Ok(Arc::new(LifecycleWorker {
            id: WorkerId::new(),
            profile: PathBuf::from(format!("/profiles/{}", session_id.0)),
            closed: self.closed.clone(),
        }))
    }
}

#[tokio::test]
async fn session_owns_worker_from_creation_through_delete() {
    let closed = Arc::new(AtomicBool::new(false));
    let pool = Arc::new(WorkerPool::new(
        8,
        Arc::new(LifecycleFactory {
            closed: closed.clone(),
        }),
    ));
    let manager = SessionManager::new(pool.clone());
    let session = manager
        .create(CreateSessionRequest {
            profile: "default".into(),
            proxy: None,
            execution_policy: Default::default(),
            zigzagzig: false,
        })
        .await
        .unwrap();

    assert_eq!(pool.active_workers().await, 1);
    manager.delete(&session.id).await.unwrap();
    assert_eq!(pool.active_workers().await, 0);
    assert!(closed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn thirty_two_warm_sessions_remain_addressable_with_eight_active_slots() {
    let pool = Arc::new(WorkerPool::new(
        8,
        Arc::new(LifecycleFactory {
            closed: Arc::new(AtomicBool::new(false)),
        }),
    ));
    let manager = SessionManager::new(pool.clone());
    let mut created = Vec::new();
    for index in 0..32 {
        created.push(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                manager.create(CreateSessionRequest {
                    profile: format!("warm-{index}"),
                    proxy: None,
                    execution_policy: Default::default(),
                    zigzagzig: false,
                }),
            )
            .await
            .expect("warm session creation retained an active-work permit")
            .unwrap(),
        );
    }
    assert_eq!(manager.list().await.len(), 32);
    assert_eq!(pool.active_workers().await, 32);
    for session in created {
        assert_eq!(
            manager.get(&session.id).await.unwrap().profile,
            session.profile
        );
    }
}

struct FailingCloseWorker {
    id: WorkerId,
}

#[async_trait]
impl BrowserWorker for FailingCloseWorker {
    fn worker_id(&self) -> WorkerId {
        self.id.clone()
    }
    fn profile_dir(&self) -> &Path {
        Path::new("/profiles/failing")
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
        Err(CommandError {
            code: types::ErrorCode::BrowserCommandFailed,
            message: "close refused".into(),
            layer: types::ErrorLayer::Driver,
            retryable: true,
        })
    }
}

struct FailingCloseFactory;

#[async_trait]
impl WorkerFactory for FailingCloseFactory {
    async fn launch(&self, _: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        Ok(Arc::new(FailingCloseWorker {
            id: WorkerId::new(),
        }))
    }
}

#[tokio::test]
async fn failed_release_keeps_the_session_registered_for_retry() {
    let pool = Arc::new(WorkerPool::new(8, Arc::new(FailingCloseFactory)));
    let manager = SessionManager::new(pool);
    let session = manager
        .create(CreateSessionRequest {
            profile: "default".into(),
            proxy: None,
            execution_policy: Default::default(),
            zigzagzig: false,
        })
        .await
        .unwrap();

    // The close failure must not unregister the session: deleting first
    // would leak the browser with no handle left to close it.
    assert!(manager.delete(&session.id).await.is_err());
    assert!(manager.get(&session.id).await.is_ok());
}
