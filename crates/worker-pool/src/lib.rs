mod chromium;
mod fingerprint_host;
mod form_snapshot;
mod har;
mod network_quiet;
pub mod process_registry;
mod selection;
mod skill_adapter;
mod targeting;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use network_engine::state::{HttpStateSnapshot, ResponseStateDelta};
use tokio::sync::{Mutex, OnceCell, OwnedRwLockReadGuard, OwnedSemaphorePermit, RwLock, Semaphore};
use types::{
    CaptureScreenshotCommand, ClickAndWaitForDownloadCommand, ClickAndWaitForPopupCommand,
    ClickCommand, ClosePageCommand, CommandError, EvaluateJavaScriptCommand, Evidence,
    InspectCommand, ListPagesCommand, NavigateCommand, OpenPageCommand, PageId, SessionId,
    SetEmulatedMediaCommand, SetFocusEmulationCommand, TargetSpec, TypeTextCommand,
    UploadFilesCommand, WaitForCommand, WorkerId,
};

pub use chromium::ChromiumWorkerFactory;
pub use fingerprint_host::ChromiumPageHost;
pub use form_snapshot::{
    control_action_evidence, decode_form_snapshot, form_snapshot_expression,
    form_snapshot_expression_with_limit, validate_control_action,
};
pub use har::{har_document, HarEntry, HarRecorder};
pub use selection::{
    BrowserWorkerSelector, EnginePreference, FactoryRegistration, RequiredCapabilities,
    SelectedWorkerFactory, DEFAULT_REPLACEMENT_CLEANUP_TIMEOUT,
};
pub use skill_adapter::{
    skill_engine, ChromiumSkillAdapter, FirefoxSkillAdapter,
    CHROMIUM_PRODUCTION_SKILL_PROFILE_VERSION, FIREFOX_PRODUCTION_SKILL_PROFILE_VERSION,
    PRODUCTION_SKILL_CAPABILITIES,
};

/// Adds command-ready semantic targets to actionable accessibility nodes.
/// Duplicate role/name pairs receive an ordinal in tree traversal order,
/// matching the candidate collection order used by the resolver.
pub fn annotate_accessibility_targets(nodes: &mut [types::AccessibilityNode]) {
    let mut totals = BTreeMap::new();
    count_accessibility_targets(nodes, &mut totals);
    annotate_accessibility_targets_with_totals(nodes, &totals);
}

pub(crate) fn accessibility_role_is_actionable(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "checkbox"
            | "combobox"
            | "link"
            | "listbox"
            | "radio"
            | "searchbox"
            | "slider"
            | "spinbutton"
            | "switch"
            | "textbox"
    )
}

fn count_accessibility_targets(
    nodes: &[types::AccessibilityNode],
    totals: &mut BTreeMap<(String, String), usize>,
) {
    for node in nodes {
        if let (Some(role), Some(name)) = (&node.role, &node.name) {
            if accessibility_role_is_actionable(role) && !name.is_empty() && name != "[redacted]" {
                *totals.entry((role.clone(), name.clone())).or_default() += 1;
            }
        }
        count_accessibility_targets(&node.children, totals);
    }
}

pub(crate) fn annotate_accessibility_targets_with_totals(
    nodes: &mut [types::AccessibilityNode],
    totals: &BTreeMap<(String, String), usize>,
) {
    fn annotate(
        nodes: &mut [types::AccessibilityNode],
        totals: &BTreeMap<(String, String), usize>,
        seen: &mut BTreeMap<(String, String), usize>,
    ) {
        for node in nodes {
            if let (Some(role), Some(name)) = (&node.role, &node.name) {
                let key = (role.clone(), name.clone());
                if accessibility_role_is_actionable(role)
                    && !name.is_empty()
                    && name != "[redacted]"
                {
                    let index = seen.entry(key.clone()).or_default();
                    if node.target.is_none() {
                        node.target = Some(types::AccessibilityTarget {
                            role: role.clone(),
                            accessible_name: name.clone(),
                            ordinal: (totals.get(&key).copied().unwrap_or_default() > 1)
                                .then_some(*index),
                        });
                    }
                    *index += 1;
                }
            }
            annotate(&mut node.children, totals, seen);
        }
    }

    annotate(nodes, totals, &mut BTreeMap::new());
}

pub fn session_download_dir(root: &Path, session_id: &SessionId) -> PathBuf {
    root.join(session_id.0.to_string())
}

pub fn resolve_upload_paths(
    roots: &[PathBuf],
    paths: &[PathBuf],
) -> Result<Vec<PathBuf>, CommandError> {
    let cwd = std::env::current_dir()
        .map(|dir| dir.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_owned());
    let roots = roots
        .iter()
        .map(|root| {
            std::fs::canonicalize(root).map_err(|error| {
                policy_error(format!(
                    "invalid upload root {}: {error} (relative roots resolve against the gateway working directory {cwd})",
                    root.display()
                ))
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
                    "upload path is outside configured roots: {} (roots: {})",
                    path.display(),
                    roots
                        .iter()
                        .map(|root| root.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
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
    /// Toggle fingerprint spoofing. Implementations that register preload
    /// scripts should apply/remove them immediately (not only on next page).
    async fn set_fingerprint_enabled(&self, _enabled: bool) -> Result<(), CommandError> {
        Ok(())
    }
    /// Whether fingerprint spoofing is currently enabled.
    fn fingerprint_enabled(&self) -> bool {
        false
    }
    /// Toggle human-like input synthesis (`behavioral-engine`). Engines with no
    /// synthesizer accept the call and stay direct (the default below), so the
    /// executor can write session policy onto any worker.
    async fn set_humanization_enabled(&self, _enabled: bool) -> Result<(), CommandError> {
        Ok(())
    }
    /// Whether human-like input synthesis is currently enabled.
    fn humanization_enabled(&self) -> bool {
        false
    }
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
    async fn control_action(
        &self,
        _page_id: &PageId,
        _command: &types::ControlActionCommand,
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
    /// In-memory viewport PNG for machine consumers (vision assist). Unlike
    /// `capture_screenshot`, no artifact is persisted and no evidence emitted.
    async fn network_log(
        &self,
        _page_id: &PageId,
        _command: &types::NetworkLogCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn emulate(
        &self,
        _page_id: &PageId,
        _command: &types::EmulateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn handle_dialog(
        &self,
        _page_id: &PageId,
        _command: &types::HandleDialogCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn print_to_pdf(
        &self,
        _page_id: &PageId,
        _command: &types::PrintToPdfCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn get_cookies(
        &self,
        _page_id: &PageId,
        _command: &types::GetCookiesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn set_cookies(
        &self,
        _page_id: &PageId,
        _command: &types::SetCookiesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn delete_cookies(
        &self,
        _page_id: &PageId,
        _command: &types::DeleteCookiesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn screenshot_bytes(&self, _page_id: &PageId) -> Result<Vec<u8>, CommandError> {
        Err(unsupported_error())
    }

    async fn a11y_snapshot(
        &self,
        _page_id: &PageId,
        _command: &types::AccessibilitySnapshotCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn form_snapshot(
        &self,
        _page_id: &PageId,
        _max_controls: Option<u32>,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn activate_page(
        &self,
        _command: &types::ActivatePageCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
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
    // ChromiumWorker overrides this via chromiumoxide EvaluateParams, bounded by
    // `timeout_ms` and result-shaped through `js_engine::bound_result`. Every other
    // worker keeps this default and refuses JS execution.
    async fn evaluate_javascript(
        &self,
        _page_id: &PageId,
        _command: &EvaluateJavaScriptCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }
    /// Whether this worker mirrors page HTTP state, and can therefore serve the
    /// direct-HTTP execution path. Workers that keep the `http_state` default below
    /// must report `false`, so the adaptive executor routes the command through the
    /// browser instead of failing it on an unsupported primitive.
    fn supports_http_state(&self) -> bool {
        false
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

    fn can_select(&self, _preference: &EnginePreference) -> bool {
        false
    }

    async fn replace_session(
        &self,
        _session_id: &SessionId,
        _preference: &EnginePreference,
    ) -> Result<(), CommandError> {
        Err(policy_error(
            "worker factory does not support session replacement",
        ))
    }
}

#[derive(Clone)]
pub struct WorkerPool {
    inner: Arc<PoolInner>,
}

struct PoolInner {
    factory: Arc<dyn WorkerFactory>,
    permits: Arc<Semaphore>,
    entries: Mutex<HashMap<SessionId, Arc<WorkerEntry>>>,
    // Lifecycle lock order is session gate -> lease permit -> entries/factory.
    // The registry mutex is released before waiting on a session gate.
    session_gates: Mutex<HashMap<SessionId, Weak<RwLock<()>>>>,
    replacement_cleanup_timeout: std::time::Duration,
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
    _session_use: Arc<OwnedRwLockReadGuard<()>>,
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
        Self::with_replacement_timeout(max_active, factory, DEFAULT_REPLACEMENT_CLEANUP_TIMEOUT)
    }

    pub fn with_replacement_timeout(
        max_active: usize,
        factory: Arc<dyn WorkerFactory>,
        replacement_cleanup_timeout: std::time::Duration,
    ) -> Self {
        assert!(max_active > 0, "worker pool capacity must be positive");
        Self {
            inner: Arc::new(PoolInner {
                factory,
                permits: Arc::new(Semaphore::new(max_active)),
                entries: Mutex::new(HashMap::new()),
                session_gates: Mutex::new(HashMap::new()),
                replacement_cleanup_timeout,
            }),
        }
    }

    pub async fn lease(&self, session_id: SessionId) -> Result<WorkerLease, CommandError> {
        // Worker identity and profile stay warm across calls; the fair semaphore
        // bounds only operations actively using a worker. Owned permits are
        // cancellation-safe and return on finish, error, or abort.
        let session_gate = self.session_gate(&session_id).await;
        let session_use = session_gate.read_owned().await;
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
            result.map(|worker| (worker, active_permit, session_use))
        })
        .await
        .map_err(|error| resource_error(format!("worker launch task failed: {error}")))?;
        cancellation.armed = false;

        match result {
            Ok((worker, active_permit, session_use)) => {
                tracing::info!(session_id = %session_id.0, "worker.leased");
                Ok(WorkerLease {
                    worker,
                    _active_permit: Arc::new(active_permit),
                    _session_use: Arc::new(session_use),
                })
            }
            Err(error) => Err(error),
        }
    }

    pub async fn release_session(&self, session_id: &SessionId) -> Result<(), CommandError> {
        self.cleanup_session(session_id, false).await
    }

    pub async fn invalidate_session(&self, session_id: &SessionId) -> Result<(), CommandError> {
        self.cleanup_session(session_id, true).await
    }

    pub fn can_select(&self, preference: &EnginePreference) -> bool {
        self.inner.factory.can_select(preference)
    }

    pub async fn replace_session(
        &self,
        session_id: &SessionId,
        preference: &EnginePreference,
    ) -> Result<(), CommandError> {
        if !self.can_select(preference) {
            return Err(policy_error(
                "no browser worker satisfies the requested replacement preference",
            ));
        }
        let session_gate = self.session_gate(session_id).await;
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.clone();
        let preference = preference.clone();
        let mut cleanup = tokio::spawn(async move {
            let _session_exclusive = session_gate.write_owned().await;
            let entry = inner.entries.lock().await.remove(&session_id);
            if let Some(worker) = entry.and_then(|entry| entry.worker.get().cloned()) {
                worker.terminate().await?;
            }
            inner
                .factory
                .replace_session(&session_id, &preference)
                .await
        });
        match tokio::time::timeout(self.inner.replacement_cleanup_timeout, &mut cleanup).await {
            Ok(result) => result.map_err(|error| {
                resource_error(format!("worker replacement task failed: {error}"))
            })?,
            Err(_) => Err(replacement_timeout_error()),
        }
    }

    pub async fn wait_for_session_stable(&self, session_id: &SessionId) {
        let session_gate = self.session_gate(session_id).await;
        drop(session_gate.write_owned().await);
    }

    async fn cleanup_session(
        &self,
        session_id: &SessionId,
        terminate: bool,
    ) -> Result<(), CommandError> {
        let session_gate = self.session_gate(session_id).await;
        let factory = Arc::clone(&self.inner.factory);
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.clone();
        tokio::spawn(async move {
            let _session_exclusive = session_gate.write_owned().await;
            let entry = inner.entries.lock().await.remove(&session_id);
            let result = if let Some(entry) = entry {
                if let Some(worker) = entry.worker.get() {
                    if terminate {
                        worker.terminate().await
                    } else {
                        worker.close().await
                    }
                } else {
                    Ok(())
                }
            } else {
                Ok(())
            };
            factory.release_session(&session_id).await;
            if result.is_ok() {
                let action = if terminate {
                    "worker.invalidated"
                } else {
                    "worker.released"
                };
                tracing::info!(session_id = %session_id.0, action);
            }
            result
        })
        .await
        .map_err(|error| resource_error(format!("worker cleanup task failed: {error}")))?
    }

    async fn session_gate(&self, session_id: &SessionId) -> Arc<RwLock<()>> {
        let mut gates = self.inner.session_gates.lock().await;
        if let Some(gate) = gates.get(session_id).and_then(Weak::upgrade) {
            return gate;
        }
        gates.retain(|_, gate| gate.strong_count() > 0);
        let gate = Arc::new(RwLock::new(()));
        gates.insert(session_id.clone(), Arc::downgrade(&gate));
        gate
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

fn replacement_timeout_error() -> CommandError {
    CommandError {
        code: types::ErrorCode::DeadlineExceeded,
        message: "browser worker replacement cleanup exceeded its deadline".into(),
        layer: types::ErrorLayer::Driver,
        retryable: true,
    }
}
