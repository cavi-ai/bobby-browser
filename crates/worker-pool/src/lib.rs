mod chromium;
mod targeting;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use network_engine::state::{HttpStateSnapshot, ResponseStateDelta};
use tokio::sync::{Mutex, OnceCell, OwnedSemaphorePermit, Semaphore};
use types::{
    CaptureScreenshotCommand, ClickAndWaitForDownloadCommand, ClickAndWaitForPopupCommand,
    ClickCommand, ClosePageCommand, CommandError, Evidence, InspectCommand, ListPagesCommand,
    NavigateCommand, OpenPageCommand, PageId, SessionId, SetEmulatedMediaCommand,
    SetFocusEmulationCommand, TypeTextCommand, UploadFilesCommand, WaitForCommand, WorkerId,
};

pub use chromium::ChromiumWorkerFactory;

pub fn session_download_dir(root: &Path, session_id: &SessionId) -> PathBuf {
    root.join(session_id.0.to_string())
}

pub fn resolve_upload_paths(
    roots: &[PathBuf],
    paths: &[PathBuf],
) -> Result<Vec<PathBuf>, CommandError> {
    let roots = roots
        .iter()
        .map(|root| {
            std::fs::canonicalize(root).map_err(|error| {
                policy_error(format!("invalid upload root {}: {error}", root.display()))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    paths
        .iter()
        .map(|path| {
            let canonical = std::fs::canonicalize(path).map_err(|error| {
                policy_error(format!("invalid upload file {}: {error}", path.display()))
            })?;
            if !canonical.is_file() {
                return Err(policy_error(format!(
                    "upload path is not a file: {}",
                    path.display()
                )));
            }
            if !roots.iter().any(|root| canonical.starts_with(root)) {
                return Err(policy_error(format!(
                    "upload path is outside configured roots: {}",
                    path.display()
                )));
            }
            Ok(canonical)
        })
        .collect()
}

fn policy_error(message: impl Into<String>) -> CommandError {
    CommandError {
        code: types::ErrorCode::PolicyDenied,
        message: message.into(),
        layer: types::ErrorLayer::Driver,
        retryable: false,
    }
}

#[async_trait]
pub trait BrowserWorker: Send + Sync {
    fn worker_id(&self) -> WorkerId;
    fn profile_dir(&self) -> &Path;
    async fn open_page(&self, page_id: PageId) -> Result<(), CommandError>;
    async fn navigate(
        &self,
        page_id: &PageId,
        command: &NavigateCommand,
    ) -> Result<Vec<Evidence>, CommandError>;
    async fn inspect(
        &self,
        page_id: &PageId,
        command: &InspectCommand,
    ) -> Result<Vec<Evidence>, CommandError>;
    async fn click(
        &self,
        page_id: &PageId,
        command: &ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError>;
    async fn type_text(
        &self,
        page_id: &PageId,
        command: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError>;
    async fn upload_files(
        &self,
        _page_id: &PageId,
        _command: &UploadFilesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }
    async fn open_page_command(
        &self,
        _command: &OpenPageCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }
    async fn list_pages(&self, _command: &ListPagesCommand) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }
    async fn close_page_command(
        &self,
        _command: &ClosePageCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }
    async fn click_and_wait_for_popup(
        &self,
        _page_id: &PageId,
        _command: &ClickAndWaitForPopupCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }
    async fn click_and_wait_for_download(
        &self,
        _page_id: &PageId,
        _command: &ClickAndWaitForDownloadCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }
    async fn wait_for(
        &self,
        _page_id: &PageId,
        _command: &WaitForCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }
    async fn capture_screenshot(
        &self,
        _page_id: &PageId,
        _command: &CaptureScreenshotCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }
    async fn set_focus_emulation(
        &self,
        _page_id: &PageId,
        _command: &SetFocusEmulationCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }
    async fn set_emulated_media(
        &self,
        _page_id: &PageId,
        _command: &SetEmulatedMediaCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }
    async fn http_state(&self, _page_id: &PageId) -> Result<HttpStateSnapshot, CommandError> {
        Err(unsupported_error())
    }
    async fn commit_http_state(
        &self,
        _page_id: &PageId,
        _expected_version: u64,
        _delta: ResponseStateDelta,
    ) -> Result<(), CommandError> {
        Err(unsupported_error())
    }
    async fn close(&self) -> Result<(), CommandError>;
    async fn terminate(&self) -> Result<(), CommandError> {
        self.close().await
    }
}

fn unsupported_error() -> CommandError {
    CommandError {
        code: types::ErrorCode::BrowserCommandFailed,
        message: "browser primitive is not supported by this worker".into(),
        layer: types::ErrorLayer::Driver,
        retryable: false,
    }
}

#[async_trait]
pub trait WorkerFactory: Send + Sync {
    async fn launch(&self, session_id: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError>;
}

#[derive(Clone)]
pub struct WorkerPool {
    inner: Arc<PoolInner>,
}

struct PoolInner {
    factory: Arc<dyn WorkerFactory>,
    permits: Arc<Semaphore>,
    entries: Mutex<HashMap<SessionId, Arc<WorkerEntry>>>,
}

struct WorkerEntry {
    worker: OnceCell<Arc<dyn BrowserWorker>>,
    permit: Mutex<Option<OwnedSemaphorePermit>>,
}

impl WorkerEntry {
    fn new() -> Self {
        Self {
            worker: OnceCell::new(),
            permit: Mutex::new(None),
        }
    }
}

#[derive(Clone)]
pub struct WorkerLease {
    worker: Arc<dyn BrowserWorker>,
}

impl WorkerLease {
    pub fn worker_id(&self) -> WorkerId {
        self.worker.worker_id()
    }

    pub fn profile_dir(&self) -> &Path {
        self.worker.profile_dir()
    }

    pub fn worker(&self) -> &Arc<dyn BrowserWorker> {
        &self.worker
    }
}

impl WorkerPool {
    pub fn new(max_active: usize, factory: Arc<dyn WorkerFactory>) -> Self {
        assert!(max_active > 0, "worker pool capacity must be positive");
        Self {
            inner: Arc::new(PoolInner {
                factory,
                permits: Arc::new(Semaphore::new(max_active)),
                entries: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub async fn lease(&self, session_id: SessionId) -> Result<WorkerLease, CommandError> {
        let entry = {
            let mut entries = self.inner.entries.lock().await;
            entries
                .entry(session_id.clone())
                .or_insert_with(|| Arc::new(WorkerEntry::new()))
                .clone()
        };

        let factory = self.inner.factory.clone();
        let permits = self.inner.permits.clone();
        let entry_for_init = entry.clone();
        let session_for_init = session_id.clone();
        let result = entry
            .worker
            .get_or_try_init(|| async move {
                let permit = permits.acquire_owned().await.map_err(|_| {
                    resource_error("worker pool is shutting down; no new leases are available")
                })?;
                let worker = factory.launch(&session_for_init).await?;
                *entry_for_init.permit.lock().await = Some(permit);
                Ok(worker)
            })
            .await;

        match result {
            Ok(worker) => Ok(WorkerLease {
                worker: worker.clone(),
            }),
            Err(error) => {
                let mut entries = self.inner.entries.lock().await;
                if entries
                    .get(&session_id)
                    .is_some_and(|current| Arc::ptr_eq(current, &entry))
                {
                    entries.remove(&session_id);
                }
                Err(error)
            }
        }
    }

    pub async fn release_session(&self, session_id: &SessionId) -> Result<(), CommandError> {
        let entry = self.inner.entries.lock().await.remove(session_id);
        if let Some(entry) = entry {
            if let Some(worker) = entry.worker.get() {
                worker.close().await?;
            }
            entry.permit.lock().await.take();
        }
        Ok(())
    }

    pub async fn invalidate_session(&self, session_id: &SessionId) -> Result<(), CommandError> {
        let entry = self.inner.entries.lock().await.remove(session_id);
        let Some(entry) = entry else {
            return Ok(());
        };
        entry.permit.lock().await.take();
        if let Some(worker) = entry.worker.get() {
            worker.terminate().await?;
        }
        Ok(())
    }

    pub async fn active_workers(&self) -> usize {
        self.inner
            .entries
            .lock()
            .await
            .values()
            .filter(|entry| entry.worker.initialized())
            .count()
    }
}

fn resource_error(message: impl Into<String>) -> CommandError {
    CommandError {
        code: types::ErrorCode::ResourceExhausted,
        message: message.into(),
        layer: types::ErrorLayer::Driver,
        retryable: true,
    }
}
