mod chromium;
mod engine;
mod page_behavior;
pub use engine::*;
pub use page_behavior::PageBehavior;
mod fingerprint_host;
mod form_snapshot;
mod har;
pub mod navigation_settle;
mod network_quiet;
pub mod policy;
pub mod process_registry;
pub mod secret_material;
mod selection;
mod skill_adapter;
mod targeting;
pub mod upload;
pub mod wait;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use tokio::sync::{Mutex, OnceCell, OwnedRwLockReadGuard, OwnedSemaphorePermit, RwLock, Semaphore};
use types::{CommandError, Evidence, SessionId, WorkerId};

pub use chromium::{is_dead_worker_error, ChromiumWorkerFactory};

/// The Firefox worker's message once its transport is gone or it was closed.
pub const FIREFOX_WORKER_CLOSED_MESSAGE: &str = "Firefox companion worker is closed";

/// A Firefox BiDi transport died. Chromium's closed-page check uses this so
/// the Chromium module does not own Firefox's wording.
pub fn is_firefox_bidi_transport_dead(message: &str) -> bool {
    message.contains("Firefox BiDi")
        && !message.contains("client closed")
        && (message.contains("connection closed")
            || message.contains("connection ended")
            || message.contains("disconnected")
            || message.contains("command channel closed")
            || message.contains("command capacity closed")
            || message.contains("response channel closed"))
}

/// What a caller is told when its session's browser is gone.
pub const BROWSER_GONE_MESSAGE: &str =
    "this session's browser is gone; create a new session and close this one";

/// The session's browser can never serve another command: a dead worker
/// (either engine) or a closed Firefox worker.
pub fn is_browser_gone_error(error: &types::CommandError) -> bool {
    is_dead_worker_error(error) || error.message == FIREFOX_WORKER_CLOSED_MESSAGE
}
pub use fingerprint_host::ChromiumPageHost;
pub use form_snapshot::{
    control_action_evidence, decode_form_snapshot, form_snapshot_expression,
    form_snapshot_expression_with_limit, target_specs_equivalent, validate_control_action,
};
pub use har::{har_document, HarEntry, HarRecorder};
pub use network_quiet::{
    bounded_request_id, counted_in_flight, map_bidi_network_type, pending_page_loads,
    warn_tracking_lost, NetworkQuietFilters, NetworkQuietState, TrackingLoss, MAX_TRACKED_REQUESTS,
};
pub use selection::{
    BrowserWorkerSelector, EnginePreference, FactoryRegistration, RequiredCapabilities,
    SelectedWorkerFactory, DEFAULT_LEASE_LAUNCH_TIMEOUT, DEFAULT_REPLACEMENT_CLEANUP_TIMEOUT,
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
    annotate_accessibility_targets_with_totals(nodes, &totals, BTreeMap::new());
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

/// `preceding` counts the targets of each role and name that come before
/// `nodes` in the page, so ordinals stay page-wide in a scoped tree.
pub(crate) fn annotate_accessibility_targets_with_totals(
    nodes: &mut [types::AccessibilityNode],
    totals: &BTreeMap<(String, String), usize>,
    preceding: BTreeMap<(String, String), usize>,
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
                            frame_path: Vec::new(),
                        });
                    }
                    *index += 1;
                }
            }
            annotate(&mut node.children, totals, seen);
        }
    }

    let mut seen = preceding;
    annotate(nodes, totals, &mut seen);
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

    async fn close(&self) -> Result<(), CommandError>;

    async fn terminate(&self) -> Result<(), CommandError> {
        self.close().await
    }

    async fn reconnect_live_process(&self) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    fn session_settings(&self) -> Option<&dyn SessionSettings> {
        None
    }

    fn tabs(&self) -> Option<&dyn TabsEngine> {
        None
    }

    fn navigation(&self) -> Option<&dyn NavigationEngine> {
        None
    }

    fn observation(&self) -> Option<&dyn ObservationEngine> {
        None
    }

    fn input(&self) -> Option<&dyn InputEngine> {
        None
    }

    fn events(&self) -> Option<&dyn EventsEngine> {
        None
    }

    fn capture(&self) -> Option<&dyn CaptureEngine> {
        None
    }

    fn page_configuration(&self) -> Option<&dyn PageConfigurationEngine> {
        None
    }

    fn javascript(&self) -> Option<&dyn JavaScriptEngine> {
        None
    }

    fn web_state(&self) -> Option<&dyn WebStateEngine> {
        None
    }

    fn wait_provider(&self) -> Option<&dyn WaitProvider> {
        None
    }
}

pub fn corpus_mask_scripts(token: &str) -> (String, String) {
    (
        corpus_mask_install_script(token),
        corpus_mask_cleanup_script(token),
    )
}

fn corpus_mask_install_script(token: &str) -> String {
    format!(
        r#"(() => {{
const token={token:?};
const attr='data-bobby-corpus-mask';
const selector='input,textarea,select,[contenteditable]:not([contenteditable="false"]),[data-secret],[data-sensitive],[autocomplete*="password" i],[autocomplete*="token" i],[name*="password" i],[name*="token" i],[name*="secret" i],[name*="api-key" i],[id*="password" i],[id*="token" i],[id*="secret" i],[id*="api-key" i]';
const css=`${{selector}}{{color:transparent!important;-webkit-text-fill-color:transparent!important;text-shadow:none!important;caret-color:transparent!important;}}`;
const cover=(doc,element)=>{{const rect=element.getBoundingClientRect();if(rect.width<=0||rect.height<=0)return;const node=doc.createElement('div');node.setAttribute(attr,token);Object.assign(node.style,{{position:'fixed',left:`${{rect.left}}px`,top:`${{rect.top}}px`,width:`${{rect.width}}px`,height:`${{rect.height}}px`,background:'#d1d5db',zIndex:'2147483647',pointerEvents:'none'}});doc.documentElement.appendChild(node);}};
const visitRoot=(root,doc)=>{{const style=doc.createElement('style');style.setAttribute(attr,token);style.textContent=css;(root===doc?(doc.head||doc.documentElement):root).appendChild(style);for(const element of root.querySelectorAll(selector))cover(doc,element);for(const element of root.querySelectorAll('*'))if(element.shadowRoot)visitRoot(element.shadowRoot,doc);}};
const visit=(doc)=>{{
  visitRoot(doc,doc);
  for(const frame of doc.querySelectorAll('iframe,frame')){{
    const rect=frame.getBoundingClientRect();
    try{{if(frame.contentDocument){{visit(frame.contentDocument);continue;}}}}catch(_error){{}}
    if(rect.width<=0||rect.height<=0)continue;
    const cover=doc.createElement('div');cover.setAttribute(attr,token);Object.assign(cover.style,{{position:'fixed',left:`${{rect.left}}px`,top:`${{rect.top}}px`,width:`${{rect.width}}px`,height:`${{rect.height}}px`,background:'#d1d5db',zIndex:'2147483647',pointerEvents:'none'}});doc.documentElement.appendChild(cover);
  }}
}};
visit(document);
return true;
}})()"#
    )
}

fn corpus_mask_cleanup_script(token: &str) -> String {
    format!(
        r#"(() => {{
const token={token:?};const attr='data-bobby-corpus-mask';
const cleanRoot=(root)=>{{for(const element of root.querySelectorAll('*'))if(element.shadowRoot)cleanRoot(element.shadowRoot);for(const node of [...root.querySelectorAll(`[${{attr}}="${{token}}"]`)])node.remove();}};
const visit=(doc)=>{{cleanRoot(doc);for(const frame of doc.querySelectorAll('iframe,frame')){{try{{if(frame.contentDocument)visit(frame.contentDocument);}}catch(_error){{}}}}}};
visit(document);return true;
}})()"#
    )
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

    /// Graceful teardown on server shutdown: factories holding shared browser
    /// resources (e.g. the single Firefox BiDi WebDriver session) must end
    /// them here — process exit without this leaks the session in the
    /// browser, which then refuses every later `session.new`.
    async fn shutdown(&self) {}

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
    /// Hard outer bound around `factory.launch` during first lease. Without this,
    /// a hung companion/BiDi launch can sit until the host MCP client empties the
    /// tool result (~80-90s) with no structured error.
    lease_launch_timeout: std::time::Duration,
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
        Self::with_timeouts(
            max_active,
            factory,
            DEFAULT_REPLACEMENT_CLEANUP_TIMEOUT,
            DEFAULT_LEASE_LAUNCH_TIMEOUT,
        )
    }

    pub fn with_replacement_timeout(
        max_active: usize,
        factory: Arc<dyn WorkerFactory>,
        replacement_cleanup_timeout: std::time::Duration,
    ) -> Self {
        Self::with_timeouts(
            max_active,
            factory,
            replacement_cleanup_timeout,
            DEFAULT_LEASE_LAUNCH_TIMEOUT,
        )
    }

    pub fn with_timeouts(
        max_active: usize,
        factory: Arc<dyn WorkerFactory>,
        replacement_cleanup_timeout: std::time::Duration,
        lease_launch_timeout: std::time::Duration,
    ) -> Self {
        assert!(max_active > 0, "worker pool capacity must be positive");
        assert!(
            !lease_launch_timeout.is_zero(),
            "lease launch timeout must be positive"
        );
        Self {
            inner: Arc::new(PoolInner {
                factory,
                permits: Arc::new(Semaphore::new(max_active)),
                entries: Mutex::new(HashMap::new()),
                session_gates: Mutex::new(HashMap::new()),
                replacement_cleanup_timeout,
                lease_launch_timeout,
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
        let mut launch_task = tokio::spawn(async move {
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
        });

        let result =
            match tokio::time::timeout(self.inner.lease_launch_timeout, &mut launch_task).await {
                Ok(join_result) => {
                    cancellation.armed = false;
                    join_result.map_err(|error| {
                        resource_error(format!("worker launch task failed: {error}"))
                    })?
                }
                Err(_) => {
                    // The caller gets its answer now, but the launch keeps
                    // running: `cancellation` stays armed, so a late worker is
                    // terminated and the task runs its own failure cleanup.
                    // Only a launch still hung after a second deadline is
                    // aborted.
                    self.reap_hung_launch(launch_task, session_id.clone(), entry);
                    return Err(lease_launch_deadline_error(self.inner.lease_launch_timeout));
                }
            };

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

    fn reap_hung_launch<T: Send + 'static>(
        &self,
        mut launch_task: tokio::task::JoinHandle<T>,
        session_id: SessionId,
        entry: Arc<WorkerEntry>,
    ) {
        let pool = self.clone();
        tokio::spawn(async move {
            let grace = pool.inner.lease_launch_timeout;
            if tokio::time::timeout(grace, &mut launch_task).await.is_ok() {
                return;
            }
            launch_task.abort();
            let _ = launch_task.await;
            // The exclusive gate waits out any concurrent lease that took over
            // this entry's initialization, so an entry still uninitialized
            // here has no launch in flight and is safe to scrub.
            let session_gate = pool.session_gate(&session_id).await;
            let _session_exclusive = session_gate.write_owned().await;
            let scrubbed = {
                let mut entries = pool.inner.entries.lock().await;
                let stale = entries.get(&session_id).is_some_and(|current| {
                    Arc::ptr_eq(current, &entry) && !current.worker.initialized()
                });
                stale && entries.remove(&session_id).is_some()
            };
            if scrubbed {
                pool.inner.factory.release_session(&session_id).await;
            }
            tracing::warn!(session_id = %session_id.0, scrubbed, "worker.launch_reaped");
        });
    }

    pub async fn release_session(&self, session_id: &SessionId) -> Result<(), CommandError> {
        self.cleanup_session(session_id, false).await
    }

    pub async fn invalidate_session(&self, session_id: &SessionId) -> Result<(), CommandError> {
        self.cleanup_session(session_id, true).await
    }

    /// Invalidates `session_id` only while it still refers to `worker_id`.
    /// A recovery attempt may arrive after another caller has already installed
    /// a healthy replacement; that stale attempt must leave the replacement
    /// and its factory-owned session state intact.
    pub async fn invalidate_session_if_worker(
        &self,
        session_id: &SessionId,
        worker_id: &WorkerId,
    ) -> Result<(), CommandError> {
        let session_gate = self.session_gate(session_id).await;
        let factory = Arc::clone(&self.inner.factory);
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.clone();
        let worker_id = worker_id.clone();
        tokio::spawn(async move {
            let _session_exclusive = session_gate.write_owned().await;
            let entry = {
                let mut entries = inner.entries.lock().await;
                let matches_failed_worker = entries
                    .get(&session_id)
                    .and_then(|entry| entry.worker.get())
                    .is_some_and(|worker| worker.worker_id() == worker_id);
                matches_failed_worker
                    .then(|| entries.remove(&session_id))
                    .flatten()
            };
            let Some(entry) = entry else {
                return Ok(());
            };
            let result = if let Some(worker) = entry.worker.get() {
                worker.terminate().await
            } else {
                Ok(())
            };
            factory.release_session(&session_id).await;
            if result.is_ok() {
                tracing::info!(session_id = %session_id.0, worker_id = %worker_id.0, "worker.invalidated");
            }
            result
        })
        .await
        .map_err(|error| resource_error(format!("worker cleanup task failed: {error}")))?
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

fn lease_launch_deadline_error(timeout: std::time::Duration) -> CommandError {
    CommandError {
        // BrowserLaunchFailed (not bare DeadlineExceeded) so session_manager and
        // MCP keep the allowlisted "browser launch failed:" diagnostic prefix.
        code: types::ErrorCode::BrowserLaunchFailed,
        message: format!(
            "browser worker launch exceeded its {}s outer deadline before factory.launch completed; run `bobby doctor` for companion/BiDi readiness",
            timeout.as_secs().max(1)
        ),
        layer: types::ErrorLayer::Driver,
        retryable: true,
    }
}

#[cfg(test)]
mod corpus_privacy_tests {
    use super::*;

    #[test]
    fn mask_scripts_cover_editable_and_credential_regions() {
        let (install, cleanup) = corpus_mask_scripts("test-token");
        for selector in [
            "input",
            "textarea",
            "select",
            "contenteditable",
            "password",
            "token",
            "api-key",
        ] {
            assert!(install.contains(selector), "missing selector {selector}");
        }
        assert!(install.contains("iframe,frame"));
        assert!(install.contains("shadowRoot"));
        assert!(install.contains("test-token"));
        assert!(cleanup.contains("test-token"));
        assert!(cleanup.contains("remove()"));
    }
}
