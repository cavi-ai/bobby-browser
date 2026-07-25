mod chromium;
mod network_quiet;
pub mod process_registry;
mod selection;
mod targeting;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use network_engine::state::{HttpStateSnapshot, ResponseStateDelta};
use tokio::sync::{Mutex, OnceCell, OwnedSemaphorePermit, Semaphore};
use types::{
    CaptureScreenshotCommand, ClickAndWaitForDownloadCommand, ClickAndWaitForPopupCommand,
    ClickCommand, ClosePageCommand, CommandError, EvaluateJavaScriptCommand, Evidence,
    InspectCommand, ListPagesCommand, NavigateCommand, OpenPageCommand, PageId, SessionId,
    SetEmulatedMediaCommand, SetFocusEmulationCommand, TargetSpec, TypeTextCommand,
    UploadFilesCommand, WaitForCommand, WorkerId,
};

pub use chromium::ChromiumWorkerFactory;
pub use selection::{
    BrowserWorkerSelector, EnginePreference, FactoryRegistration, RequiredCapabilities,
    SelectedWorkerFactory,
};

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
    /// Coordinate click used by vision fallback proposals.
    async fn click_xy(
        &self,
        _page_id: &PageId,
        _x: f64,
        _y: f64,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }
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
    async fn collect_candidates(
        &self,
        _page_id: &PageId,
        _target: &TargetSpec,
    ) -> Result<Vec<dom_engine::Candidate>, CommandError> {
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
    // ChromiumWorker overrides this (F3) via chromiumoxide EvaluateParams, bounded by
    // `timeout_ms` and result-shaped through `js_engine::bound_result`. Every other
    // worker keeps this default and refuses JS execution.
    async fn evaluate_javascript(
        &self,
        _page_id: &PageId,
        _command: &EvaluateJavaScriptCommand,
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

    async fn release_session(&self, _session_id: &SessionId) {}
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
}

struct LeaseLaunchCancellation {
    cancelled: Arc<AtomicBool>,
    armed: bool,
}

impl Drop for LeaseLaunchCancellation {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::Release);
        }
    }
}

impl WorkerEntry {
    fn new() -> Self {
        Self {
            worker: OnceCell::new(),
        }
    }
}

#[derive(Clone)]
pub struct WorkerLease {
    worker: Arc<dyn BrowserWorker>,
    _active_permit: Arc<OwnedSemaphorePermit>,
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
        // Worker identity and profile remain warm across calls, while the fair
        // semaphore bounds only operations that are actively using a worker.
        // Owned permits are cancellation-safe and return automatically when a
        // command finishes, errors, or its task is aborted.
        let active_permit = self
            .inner
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| {
                resource_error("worker pool is shutting down; no new leases are available")
            })?;
        let entry = {
            let mut entries = self.inner.entries.lock().await;
            entries
                .entry(session_id.clone())
                .or_insert_with(|| Arc::new(WorkerEntry::new()))
                .clone()
        };

        let cancelled = Arc::new(AtomicBool::new(false));
        let mut cancellation = LeaseLaunchCancellation {
            cancelled: Arc::clone(&cancelled),
            armed: true,
        };
        let inner = Arc::clone(&self.inner);
        let task_entry = Arc::clone(&entry);
        let task_session = session_id.clone();
        let result = tokio::spawn(async move {
            let factory = Arc::clone(&inner.factory);
            let launch_session = task_session.clone();
            let result = task_entry
                .worker
                .get_or_try_init(|| async move {
                    let worker = factory.launch(&launch_session).await?;
                    if cancelled.load(Ordering::Acquire) {
                        let _ = worker.terminate().await;
                        return Err(resource_error("worker launch caller was cancelled"));
                    }
                    Ok(worker)
                })
                .await
                .map(Arc::clone);
            if result.is_err() {
                {
                    let mut entries = inner.entries.lock().await;
                    if entries
                        .get(&task_session)
                        .is_some_and(|current| Arc::ptr_eq(current, &task_entry))
                    {
                        entries.remove(&task_session);
                    }
                }
                inner.factory.release_session(&task_session).await;
            }
            result.map(|worker| (worker, active_permit))
        })
        .await
        .map_err(|error| resource_error(format!("worker launch task failed: {error}")))?;
        cancellation.armed = false;

        match result {
            Ok((worker, active_permit)) => {
                tracing::info!(session_id = %session_id.0, "worker.leased");
                Ok(WorkerLease {
                    worker,
                    _active_permit: Arc::new(active_permit),
                })
            }
            Err(error) => Err(error),
        }
    }

    pub async fn release_session(&self, session_id: &SessionId) -> Result<(), CommandError> {
        let entry = self.inner.entries.lock().await.remove(session_id);
        let factory = Arc::clone(&self.inner.factory);
        let session_id = session_id.clone();
        tokio::spawn(async move {
            let result = if let Some(entry) = entry {
                if let Some(worker) = entry.worker.get() {
                    worker.close().await
                } else {
                    Ok(())
                }
            } else {
                Ok(())
            };
            factory.release_session(&session_id).await;
            if result.is_ok() {
                tracing::info!(session_id = %session_id.0, "worker.released");
            }
            result
        })
        .await
        .map_err(|error| resource_error(format!("worker cleanup task failed: {error}")))?
    }

    pub async fn invalidate_session(&self, session_id: &SessionId) -> Result<(), CommandError> {
        let entry = self.inner.entries.lock().await.remove(session_id);
        let factory = Arc::clone(&self.inner.factory);
        let session_id = session_id.clone();
        tokio::spawn(async move {
            let result = if let Some(entry) = entry {
                if let Some(worker) = entry.worker.get() {
                    worker.terminate().await
                } else {
                    Ok(())
                }
            } else {
                Ok(())
            };
            factory.release_session(&session_id).await;
            result
        })
        .await
        .map_err(|error| resource_error(format!("worker cleanup task failed: {error}")))?
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
