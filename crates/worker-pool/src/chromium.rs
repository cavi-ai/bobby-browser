use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use std::time::Duration;
use std::time::Instant;

use artifact_store::ArtifactStore;
use async_trait::async_trait;
use behavioral_engine::{
    generate_session_seed, BehavioralConfig, BezierMouseSimulator, SessionRandom, TypingAction,
    TypingSimulator,
};
use chromiumoxide::browser::{Browser, BrowserConfig as ChromiumConfig};
use chromiumoxide::cdp::browser_protocol::browser::{
    DownloadProgressState, EventDownloadProgress, EventDownloadWillBegin,
    SetDownloadBehaviorBehavior, SetDownloadBehaviorParams,
};
use chromiumoxide::cdp::browser_protocol::emulation::{
    MediaFeature, SetEmulatedMediaParams, SetFocusEmulationEnabledParams,
};
use chromiumoxide::cdp::browser_protocol::network::{
    Cookie, CookieParam, CookiePartitionKey, CookiePriority, CookieSameSite, CookieSourceScheme,
    SetCookiesParams, TimeSinceEpoch,
};
use chromiumoxide::cdp::browser_protocol::page::{CaptureScreenshotFormat, Viewport};
use chromiumoxide::cdp::browser_protocol::target::EventTargetCreated;
use chromiumoxide::cdp::js_protocol::runtime::EvaluateParams;
use chromiumoxide::layout::Point;
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::Page;
use config::BrowserConfig;
use fingerprinting::{FingerprintApplyPlan, FingerprintConfig, FingerprintHost};
use futures::StreamExt;
use network_engine::state::{
    HttpCookie, HttpCookiePartitionKey, HttpStateSnapshot, ResponseStateDelta,
};
use sha2::{Digest, Sha256};
use std::sync::atomic::AtomicBool;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use types::{
    CaptureScreenshotCommand, ClickAndWaitForDownloadCommand, ClickAndWaitForPopupCommand,
    ClickCommand, ClosePageCommand, CommandError, ControlAction, ControlActionCommand, ErrorCode,
    ErrorLayer, EvaluateJavaScriptCommand, Evidence, FormControl, FormControlTarget,
    InspectCommand, ListPagesCommand, NavigateCommand, OpenPageCommand, PageEvidence, PageId,
    ScreenshotMode, SessionId, SetEmulatedMediaCommand, SetFocusEmulationCommand, TargetSpec,
    TypeTextCommand, UploadFilesCommand, WaitCondition, WaitForCommand, WaitUntil, WorkerId,
};

use crate::{
    process_registry, resolve_upload_paths, session_download_dir,
    targeting::{
        gather_candidates, resolve_target as resolve_browser_target, resolve_target_with_visibility,
    },
    BrowserWorker, WorkerFactory,
};

#[derive(Clone)]
pub struct ChromiumWorkerFactory {
    config: BrowserConfig,
    pid_registry_dir: PathBuf,
    fingerprint: FingerprintConfig,
}

impl ChromiumWorkerFactory {
    /// Constructs a factory and reaps Chrome processes orphaned by a previous
    /// instance of this runtime. The reap runs once per process: the registry
    /// directory is shared with every other same-process worker, so sweeping
    /// it again could mistake a live sibling's registered PID for an orphan.
    pub fn new(config: BrowserConfig) -> Self {
        let pid_registry_dir = default_pid_registry_dir();
        reap_orphaned_chrome_processes_once(&pid_registry_dir);
        Self {
            config,
            pid_registry_dir,
            fingerprint: FingerprintConfig::default(),
        }
    }

    /// Same as `new`, but with an explicit PID registry directory, reaped on
    /// every call rather than behind the process-wide once guard. The
    /// directory must be exclusive to this factory, since nothing else may
    /// register a live sibling PID into it.
    pub fn with_pid_registry_dir(config: BrowserConfig, pid_registry_dir: PathBuf) -> Self {
        reap_orphaned_chrome_processes(&pid_registry_dir);
        Self {
            config,
            pid_registry_dir,
            fingerprint: FingerprintConfig::default(),
        }
    }

    pub fn with_fingerprint(mut self, fingerprint: FingerprintConfig) -> Self {
        self.fingerprint = fingerprint;
        self
    }
}

#[async_trait]
impl WorkerFactory for ChromiumWorkerFactory {
    async fn launch(&self, session_id: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        let profile_dir = self.config.profiles_dir.join(session_id.0.to_string());
        let download_dir = session_download_dir(&self.config.downloads_dir, session_id);
        tokio::fs::create_dir_all(&profile_dir)
            .await
            .map_err(|error| driver_error(ErrorCode::BrowserLaunchFailed, error))?;
        tokio::fs::create_dir_all(&download_dir)
            .await
            .map_err(|error| driver_error(ErrorCode::BrowserLaunchFailed, error))?;

        let mut builder = ChromiumConfig::builder()
            .user_data_dir(profile_dir.clone())
            .launch_timeout(Duration::from_secs(20))
            // Strip --enable-automation + disable AutomationControlled so
            // navigator.webdriver is natively false (CreepJS webDriverIsOn).
            .hide();
        if let Some(executable) = &self.config.executable {
            builder = builder.chrome_executable(executable);
        }
        if !self.config.headless {
            builder = builder.with_head();
        }
        // Chrome's sandbox requires root or unprivileged user namespaces;
        // hosted CI has neither. Honor an explicit opt-out rather than
        // guessing from uid (runners are unprivileged but still blocked).
        #[cfg(unix)]
        {
            let blocked = unsafe { libc::geteuid() } == 0
                || std::env::var_os("BOBBY_CHROME_NO_SANDBOX").is_some();
            if blocked {
                builder = builder.no_sandbox();
            }
        }
        let config = builder
            .build()
            .map_err(|error| driver_error(ErrorCode::BrowserLaunchFailed, error))?;
        let (mut browser, mut handler) = Browser::launch(config)
            .await
            .map_err(|error| driver_error(ErrorCode::BrowserLaunchFailed, error))?;
        let worker_id = WorkerId::new();
        // Best-effort: launch proceeds even if the child PID cannot be read or
        // the registry file cannot be written. This backstops future runs, it
        // is not a launch precondition.
        let pid_registry_path = browser
            .get_mut_child()
            .and_then(|child| child.as_mut_inner().id())
            .and_then(|pid| register_chrome_pid(&self.pid_registry_dir, &worker_id, pid));
        let handler_task = tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                if event.is_err() {
                    break;
                }
            }
        });

        let behavioral_config = BehavioralConfig::default().sanitize();
        Ok(Arc::new(ChromiumWorker {
            id: worker_id,
            profile_dir,
            pid_registry_path,
            upload_roots: self.config.upload_roots.clone(),
            download_dir,
            session_id: session_id.clone(),
            artifacts: ArtifactStore::new(
                self.config.artifacts_dir.clone(),
                self.config.max_artifact_bytes,
                self.config.max_screenshot_dimension,
            ),
            max_js_result_bytes: self.config.max_js_result_bytes,
            max_js_timeout_ms: self.config.max_js_timeout_ms,
            browser: Mutex::new(Some(browser)),
            pages: Mutex::new(HashMap::new()),
            network_trackers: Mutex::new(HashMap::new()),
            har_recorders: Mutex::new(HashMap::new()),
            har_tasks: Mutex::new(HashMap::new()),
            http_state: Mutex::new(HttpBridgeState::default()),
            handler_task: Mutex::new(Some(handler_task)),
            fingerprint: Mutex::new(self.fingerprint.clone()),
            fingerprint_enabled: AtomicBool::new(self.fingerprint.enabled),
            fingerprint_plan: Mutex::new(None),
            humanization_enabled: AtomicBool::new(false),
            typing_simulator: TypingSimulator::new(behavioral_config.typing),
            mouse_simulator: BezierMouseSimulator::new(behavioral_config.mouse),
            session_jitter: behavioral_config.session_jitter,
            session_random: Mutex::new(SessionRandom::new(generate_session_seed())),
        }))
    }
}

/// Shared, machine-wide directory Chromium PID registrations live in when a
/// caller does not provide an explicit one (`ChromiumWorkerFactory::new`).
fn default_pid_registry_dir() -> PathBuf {
    std::env::temp_dir().join("bobby-browser-chromium-workers")
}

/// Records `pid` as the Chrome process backing `worker_id`, so a later process
/// can recognize and reap it if this one never calls `close`/`terminate`.
/// Returns `None` on any I/O failure: recording never blocks a launch.
fn register_chrome_pid(registry_dir: &Path, worker_id: &WorkerId, pid: u32) -> Option<PathBuf> {
    process_registry::register_pid(registry_dir, &worker_id.0.to_string(), pid)
}

fn unregister_chrome_pid(path: &Path) {
    process_registry::unregister_pid(path);
}

static ORPHAN_REAP_ONCE: Once = Once::new();

/// Runs `reap_orphaned_chrome_processes` at most once per process. The registry
/// directory is shared by every worker this process launches, so sweeping it
/// again risks mistaking a live sibling worker for an orphan.
fn reap_orphaned_chrome_processes_once(registry_dir: &Path) {
    let registry_dir = registry_dir.to_path_buf();
    ORPHAN_REAP_ONCE.call_once(|| reap_orphaned_chrome_processes(&registry_dir));
}

/// Chrome-specific entry point into `process_registry::reap_orphaned_processes`.
/// Every registry entry is verified to be a Chrome/Chromium process before it is
/// killed, so a PID reused by an unrelated process is never touched.
fn reap_orphaned_chrome_processes(registry_dir: &Path) {
    process_registry::reap_orphaned_processes(
        registry_dir,
        is_running_chrome_process,
        process_registry::kill_process,
    );
}

/// Matches on a "chrom" substring rather than an exact name since the
/// Chrome/Chromium binary name varies by platform and channel
/// (`google-chrome-stable`, `Google Chrome`, `chromium`, ...).
fn is_running_chrome_process(pid: u32) -> bool {
    process_registry::process_command_name(pid).is_some_and(|name| name.contains("chrom"))
}

struct ChromiumWorker {
    id: WorkerId,
    profile_dir: PathBuf,
    /// Path of this worker's entry in the orphan-reaping PID registry
    /// (`None` if we couldn't determine the child PID at launch). Removed
    /// once `close`/`terminate` confirms the browser process is gone.
    pid_registry_path: Option<PathBuf>,
    upload_roots: Vec<PathBuf>,
    download_dir: PathBuf,
    session_id: SessionId,
    artifacts: ArtifactStore,
    max_js_result_bytes: usize,
    max_js_timeout_ms: u64,
    browser: Mutex<Option<Browser>>,
    pages: Mutex<HashMap<PageId, Page>>,
    network_trackers: Mutex<HashMap<PageId, Arc<crate::network_quiet::NetworkQuietTracker>>>,
    har_recorders: Mutex<HashMap<PageId, Arc<crate::HarRecorder>>>,
    har_tasks: Mutex<HashMap<PageId, JoinHandle<()>>>,
    http_state: Mutex<HttpBridgeState>,
    handler_task: Mutex<Option<JoinHandle<()>>>,
    fingerprint: Mutex<FingerprintConfig>,
    fingerprint_enabled: AtomicBool,
    /// Cached apply plan for the current fingerprint config (rebuilt on invalidate).
    fingerprint_plan: Mutex<Option<std::sync::Arc<FingerprintApplyPlan>>>,
    /// Whether input is synthesized through `behavioral-engine` rather than
    /// driven directly. The runtime writes `ExecutionPolicy.humanize` onto
    /// every lease, so a session that did not opt in must not be slowed down.
    humanization_enabled: AtomicBool,
    typing_simulator: TypingSimulator,
    mouse_simulator: BezierMouseSimulator,
    session_jitter: Duration,
    session_random: Mutex<SessionRandom>,
}

#[derive(Default)]
struct HttpBridgeState {
    version: u64,
    cache_validators: BTreeMap<String, String>,
}

impl ChromiumWorker {
    /// Clone the Arc-backed page handle and drop the pages guard
    /// immediately. Every command path goes through this instead of holding
    /// the mutex across CDP I/O: one hung or slow page call used to
    /// serialize (or, with no timeout, permanently stall) every page of the
    /// session and block close/terminate.
    async fn page_handle(&self, page_id: &PageId) -> Result<Page, CommandError> {
        self.pages
            .lock()
            .await
            .get(page_id)
            .cloned()
            .ok_or_else(page_missing)
    }
    fn control_target_spec(target: &FormControlTarget) -> TargetSpec {
        fn segment(segment: &types::SemanticTargetSegment) -> Box<TargetSpec> {
            Box::new(TargetSpec {
                role: Some(segment.role.clone()),
                accessible_name: Some(segment.accessible_name.clone()),
                ordinal: segment.ordinal,
                ..Default::default()
            })
        }
        TargetSpec {
            role: Some(target.role.clone()),
            accessible_name: Some(target.accessible_name.clone()),
            ordinal: target.ordinal,
            frame_path: target.frame_path.iter().map(segment).collect(),
            shadow_path: target.shadow_path.iter().map(segment).collect(),
            ..Default::default()
        }
    }

    /// Humanized click: curved approach to the target's clickable point, hover
    /// dwell, then the press, over CDP mouse events.
    async fn humanized_click(
        &self,
        page: &Page,
        resolved: &crate::targeting::ResolvedTarget,
    ) -> Result<(), CommandError> {
        let target = resolved.clickable_point(page).await?.ok_or_else(|| {
            driver_error(
                ErrorCode::BrowserCommandFailed,
                "target has no clickable point",
            )
        })?;
        let path = {
            let mut random = self.session_random.lock().await;
            self.mouse_simulator.generate_approach_path(&mut random)
        };
        let mut previous_ts = 0u64;
        for point in &path.points {
            let delta = point.timestamp_ms.saturating_sub(previous_ts);
            previous_ts = point.timestamp_ms;
            if delta > 0 {
                tokio::time::sleep(Duration::from_millis(delta)).await;
            }
            page.move_mouse(Point {
                x: target.x + point.x,
                y: target.y + point.y,
            })
            .await
            .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
        }
        if path.hover_dwell_ms > 0 {
            tokio::time::sleep(Duration::from_millis(path.hover_dwell_ms)).await;
        }
        page.click(target)
            .await
            .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
        Ok(())
    }

    /// Humanized typing: focus the target, then replay `behavioral-engine`
    /// key actions over CDP with their synthesized delays. Returns
    /// `(action_count, synthesized_ms)` for `Humanization` evidence, which is
    /// emitted only when this path ran.
    async fn humanized_type_text(
        &self,
        page: &Page,
        resolved: &crate::targeting::ResolvedTarget,
        value: &str,
        clear_first: bool,
    ) -> Result<(u32, u64), CommandError> {
        // Headless pages have no focus by default; without this the element
        // click cannot focus the input and every key event lands on <body>.
        page.activate().await.map_err(command_failed)?;
        resolved.click(page).await?;
        let actions = {
            let mut random = self.session_random.lock().await;
            let mut actions = Vec::new();
            let config = BehavioralConfig {
                session_jitter: self.session_jitter,
                ..BehavioralConfig::default()
            };
            let pause_ms =
                behavioral_engine::session_pause(&mut random, &config).as_millis() as u64;
            if pause_ms > 0 {
                actions.push(TypingAction::Pause {
                    duration_ms: pause_ms,
                });
            }
            actions.extend(self.typing_simulator.generate_with_clear(
                &mut random,
                value,
                clear_first,
            ));
            actions
        };
        // Ctrl/Cmd+A loops against Chrome's command pipeline: one chord yields
        // hundreds of phantom keydowns. Backspace over the text instead.
        let actions = if clear_first {
            let existing = resolved.value(page).await?.unwrap_or_default();
            let backspaces = u32::try_from(existing.chars().count()).unwrap_or(u32::MAX);
            actions
                .into_iter()
                .filter(|action| !matches!(action, TypingAction::SelectAll { .. }))
                .map(|action| match action {
                    TypingAction::Backspace { delay_ms, .. } => TypingAction::Backspace {
                        count: backspaces.saturating_add(1).max(1),
                        delay_ms,
                    },
                    other => other,
                })
                .collect()
        } else {
            actions
        };
        let synthesized_ms = behavioral_engine::synthesized_total_ms(&actions);
        let count = u32::try_from(actions.len()).unwrap_or(u32::MAX);
        for action in &actions {
            self.replay_typing_action(page, action).await?;
        }
        Ok((count, synthesized_ms))
    }

    async fn replay_typing_action(
        &self,
        page: &Page,
        action: &TypingAction,
    ) -> Result<(), CommandError> {
        use chromiumoxide::cdp::browser_protocol::input::{
            DispatchKeyEventParams, DispatchKeyEventType,
        };
        let delay = |ms: u64| async move {
            if ms > 0 {
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
        };
        let key_event = |key: &str, event_type: DispatchKeyEventType, modifiers: i64| {
            let is_key_down = matches!(event_type, DispatchKeyEventType::KeyDown);
            let definition = chromiumoxide::keys::get_key_definition(key);
            let mut params = DispatchKeyEventParams::builder().r#type(event_type);
            if let Some(definition) = definition {
                params = params
                    .key(definition.key)
                    .code(definition.code)
                    .windows_virtual_key_code(definition.key_code)
                    .native_virtual_key_code(definition.key_code);
            } else {
                params = params.key(key);
            }
            // Text insertion needs an explicit `text` on the keyDown, and the
            // key table leaves `text` empty for plain letters, so fall back to
            // the character itself for single printable characters. Without it
            // Chrome sees an unidentified held key: auto-repeat storms and no
            // inserted text.
            if is_key_down {
                let text = definition
                    .and_then(|definition| definition.text)
                    .or_else(|| {
                        let mut chars = key.chars();
                        match (chars.next(), chars.next()) {
                            (Some(character), None) if !character.is_control() => Some(key),
                            _ => None,
                        }
                    });
                if let Some(text) = text {
                    params = params.text(text).unmodified_text(text);
                }
                params = params.auto_repeat(false);
            }
            if modifiers != 0 {
                params = params.modifiers(modifiers);
            }
            params.build().expect("key event params are valid")
        };
        match action {
            TypingAction::KeyDown {
                character,
                delay_ms,
            } => {
                page.execute(key_event(character, DispatchKeyEventType::KeyDown, 0))
                    .await
                    .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
                delay(*delay_ms).await;
            }
            TypingAction::KeyUp {
                character,
                delay_ms,
            } => {
                page.execute(key_event(character, DispatchKeyEventType::KeyUp, 0))
                    .await
                    .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
                delay(*delay_ms).await;
            }
            TypingAction::SelectAll { delay_ms } => {
                // Cmd on macOS, Ctrl elsewhere: the browser runs on this
                // machine, so the worker's OS is the browser's OS. Both events
                // carry the modifier mask so Chrome's key state sees the chord
                // open and close. Releasing `a` unmasked leaves the modifier
                // logically held, and sending the physical modifier key storms
                // unidentified events.
                let mask = if cfg!(target_os = "macos") { 4 } else { 2 };
                page.execute(key_event("a", DispatchKeyEventType::KeyDown, mask))
                    .await
                    .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
                page.execute(key_event("a", DispatchKeyEventType::KeyUp, mask))
                    .await
                    .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
                delay(*delay_ms).await;
            }
            TypingAction::Backspace { count, delay_ms } => {
                // N backspaces at CDP speed is a sub-millisecond burst, the
                // most synthetic cadence there is. Pace the repetitions.
                for index in 0..*count {
                    page.execute(key_event("Backspace", DispatchKeyEventType::RawKeyDown, 0))
                        .await
                        .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
                    page.execute(key_event("Backspace", DispatchKeyEventType::KeyUp, 0))
                        .await
                        .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
                    if index + 1 < *count {
                        let pause = {
                            let mut random = self.session_random.lock().await;
                            random.next_f64(30.0, 90.0) as u64
                        };
                        delay(pause).await;
                    }
                }
                delay(*delay_ms).await;
            }
            TypingAction::CopyPaste { text, delay_ms } => {
                // A paste produces no key events for the pasted content.
                // Insert as text, the way the clipboard path presents it.
                delay(*delay_ms).await;
                page.execute(
                    chromiumoxide::cdp::browser_protocol::input::InsertTextParams::new(text),
                )
                .await
                .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
            }
            TypingAction::Pause { duration_ms } => {
                delay(*duration_ms).await;
            }
        }
        Ok(())
    }

    /// Lazily attaches the per-page HAR collector: three CDP network event
    /// streams merged into the page's bounded recorder, keyed by request id.
    async fn ensure_har_collector(
        &self,
        page_id: &PageId,
    ) -> Result<Arc<crate::HarRecorder>, CommandError> {
        if let Some(recorder) = self.har_recorders.lock().await.get(page_id) {
            return Ok(recorder.clone());
        }
        let page = self.page_handle(page_id).await?;
        // Single-flight the collector spawn without holding this map's guard
        // across other lock acquisitions: re-lock and re-check, so the loser
        // of the race returns the winner's recorder instead of spawning a
        // duplicate collector that splits entries between them.
        let mut recorders = self.har_recorders.lock().await;
        if let Some(recorder) = recorders.get(page_id) {
            return Ok(recorder.clone());
        }
        let recorder = Arc::new(crate::HarRecorder::default());
        let task_recorder = recorder.clone();
        let task = tokio::spawn(async move {
            use chromiumoxide::cdp::browser_protocol::network::{
                EventLoadingFailed, EventLoadingFinished, EventRequestWillBeSent,
                EventResponseReceived,
            };
            let Ok(mut will_send) = page.event_listener::<EventRequestWillBeSent>().await else {
                return;
            };
            let Ok(mut responses) = page.event_listener::<EventResponseReceived>().await else {
                return;
            };
            let Ok(mut finished) = page.event_listener::<EventLoadingFinished>().await else {
                return;
            };
            let Ok(mut failed) = page.event_listener::<EventLoadingFailed>().await else {
                return;
            };
            let mut pending: HashMap<String, crate::HarEntry> = HashMap::new();
            loop {
                tokio::select! {
                    event = will_send.next() => {
                        let Some(event) = event else { break };
                        pending.insert(event.request_id.inner().to_owned(), crate::HarEntry {
                            url: event.request.url.clone(),
                            method: event.request.method.clone(),
                            status: None,
                            started_unix_ms: *event.wall_time.inner() * 1000.0,
                            elapsed_ms: None,
                            transfer_bytes: None,
                            mime_type: None,
                            error_text: None,
                        });
                    }
                    event = responses.next() => {
                        let Some(event) = event else { break };
                        let id = event.request_id.inner().to_owned();
                        if let Some(entry) = pending.get_mut(&id) {
                            entry.status = Some(event.response.status as u16);
                            entry.mime_type = Some(event.response.mime_type.clone());
                        }
                    }
                    event = finished.next() => {
                        let Some(event) = event else { break };
                        let id = event.request_id.inner().to_owned();
                        if let Some(mut entry) = pending.remove(&id) {
                            entry.elapsed_ms = entry
                                .started_unix_ms
                                .is_finite()
                                .then(|| (*event.timestamp.inner() * 1000.0).max(0.0));
                            entry.transfer_bytes = Some(event.encoded_data_length as u64);
                            task_recorder.record(entry).await;
                        }
                    }
                    event = failed.next() => {
                        let Some(event) = event else { break };
                        let id = event.request_id.inner().to_owned();
                        if let Some(mut entry) = pending.remove(&id) {
                            entry.error_text = Some(event.error_text.clone());
                            task_recorder.record(entry).await;
                        }
                    }
                }
            }
        });
        recorders.insert(page_id.clone(), recorder.clone());
        drop(recorders);
        self.har_tasks.lock().await.insert(page_id.clone(), task);
        Ok(recorder)
    }

    async fn resolve_target(
        &self,
        page_id: &PageId,
        page: &Page,
        selector: &str,
        target: Option<&types::TargetSpec>,
    ) -> Result<crate::targeting::ResolvedTarget, CommandError> {
        let mut browser = self.browser.lock().await;
        let browser = browser.as_mut().ok_or_else(closed_error)?;
        resolve_browser_target(page_id, page, selector, target, Some(browser)).await
    }

    async fn register_page(&self, page_id: PageId, page: Page) -> Result<(), CommandError> {
        self.apply_fingerprint_to_page(&page).await?;
        let tracker = crate::network_quiet::NetworkQuietTracker::start(&page)
            .await
            .map_err(command_failed)?;
        self.network_trackers
            .lock()
            .await
            .insert(page_id.clone(), tracker);
        self.pages.lock().await.insert(page_id, page);
        Ok(())
    }

    async fn apply_fingerprint_to_page(&self, page: &Page) -> Result<(), CommandError> {
        if !self
            .fingerprint_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Ok(());
        }
        let plan = {
            let cached = self.fingerprint_plan.lock().await;
            if let Some(plan) = cached.as_ref() {
                plan.clone()
            } else {
                drop(cached);
                let config = {
                    let mut config = self.fingerprint.lock().await.clone();
                    config.enabled = true;
                    config
                };
                let plan = match FingerprintApplyPlan::from_config(&config) {
                    Ok(Some(plan)) => std::sync::Arc::new(plan),
                    Ok(None) => return Ok(()),
                    Err(error) => {
                        return Err(driver_error(
                            ErrorCode::BrowserCommandFailed,
                            error.to_string(),
                        ))
                    }
                };
                let mut cached = self.fingerprint_plan.lock().await;
                *cached = Some(plan.clone());
                plan
            }
        };
        crate::fingerprint_host::ChromiumPageHost { page }
            .apply_fingerprint(plan.as_ref())
            .await
            .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error.to_string()))
    }

    async fn unregister_page(&self, page_id: &PageId) -> Option<Page> {
        self.network_trackers.lock().await.remove(page_id);
        self.pages.lock().await.remove(page_id)
    }
}

#[async_trait]
impl BrowserWorker for ChromiumWorker {
    fn worker_id(&self) -> WorkerId {
        self.id.clone()
    }

    fn profile_dir(&self) -> &Path {
        &self.profile_dir
    }

    async fn set_fingerprint_enabled(&self, enabled: bool) -> Result<(), CommandError> {
        self.fingerprint_enabled
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
        // Drop cached plan so the next apply rebuilds with current config/enabled state.
        *self.fingerprint_plan.lock().await = None;
        Ok(())
    }

    fn fingerprint_enabled(&self) -> bool {
        self.fingerprint_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    async fn set_humanization_enabled(&self, enabled: bool) -> Result<(), CommandError> {
        self.humanization_enabled
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn humanization_enabled(&self) -> bool {
        self.humanization_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    async fn open_page(&self, page_id: PageId) -> Result<(), CommandError> {
        let mut browser_guard = self.browser.lock().await;
        let browser = browser_guard.as_mut().ok_or_else(closed_error)?;
        let page = browser
            .new_page("about:blank")
            .await
            .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
        drop(browser_guard);
        self.register_page(page_id, page).await
    }

    async fn navigate(
        &self,
        page_id: &PageId,
        command: &NavigateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let page = self.page_handle(page_id).await?;
        tokio::time::timeout(
            Duration::from_millis(command.timeout_ms),
            page.goto(command.url.as_str()),
        )
        .await
        .map_err(|_| timeout_error(command.timeout_ms))?
        .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
        let url = page
            .url()
            .await
            .map_err(command_failed)?
            .unwrap_or_default();
        let title = page
            .get_title()
            .await
            .map_err(command_failed)?
            .unwrap_or_default();
        Ok(vec![Evidence::Navigation { url, title }])
    }

    async fn inspect(
        &self,
        page_id: &PageId,
        command: &InspectCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let page = self.page_handle(page_id).await?;
        let url = page
            .url()
            .await
            .map_err(command_failed)?
            .unwrap_or_default();
        let title = page
            .get_title()
            .await
            .map_err(command_failed)?
            .unwrap_or_default();
        let (text, html, resolution) = if command.selector.is_some() || command.target.is_some() {
            let resolved = self
                .resolve_target(
                    page_id,
                    &page,
                    command.selector.as_deref().unwrap_or(""),
                    command.target.as_ref(),
                )
                .await?;
            let text = match resolved.value(&page).await {
                Ok(Some(value)) if !value.is_empty() => value,
                _ => resolved.inner_text(&page).await?.unwrap_or_default(),
            };
            let html = if command.include_html {
                Some(resolved.outer_html(&page).await?.unwrap_or_default())
            } else {
                None
            };
            (text, html, Some(resolved.evidence))
        } else {
            let body = page.find_element("body").await.map_err(command_failed)?;
            let text = body
                .inner_text()
                .await
                .map_err(command_failed)?
                .unwrap_or_default();
            let html = if command.include_html {
                Some(page.content().await.map_err(command_failed)?)
            } else {
                None
            };
            (text, html, None)
        };
        let mut evidence = vec![Evidence::Inspection {
            selector: command.selector.clone(),
            url,
            title,
            text,
            html,
        }];
        if let Some(resolution) = resolution {
            evidence.push(resolution);
        }
        Ok(evidence)
    }

    async fn click(
        &self,
        page_id: &PageId,
        command: &ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let page = self.page_handle(page_id).await?;
        let resolved = self
            .resolve_target(page_id, &page, &command.selector, command.target.as_ref())
            .await?;
        let text = resolved.inner_text(&page).await.ok().flatten();
        if self.humanization_enabled() {
            self.humanized_click(&page, &resolved).await?;
        } else {
            resolved.click(&page).await?;
        }
        Ok(vec![
            Evidence::Element {
                selector: command.selector.clone(),
                text,
            },
            resolved.evidence,
        ])
    }

    async fn click_xy(
        &self,
        page_id: &PageId,
        x: f64,
        y: f64,
    ) -> Result<Vec<Evidence>, CommandError> {
        if !x.is_finite() || !y.is_finite() {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                "vision click coordinates must be finite",
            ));
        }
        let page = self.page_handle(page_id).await?;
        page.click(Point { x, y })
            .await
            .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
        Ok(vec![Evidence::Configuration {
            name: "visionClick".into(),
            value: format!("{x},{y}"),
        }])
    }

    async fn type_text(
        &self,
        page_id: &PageId,
        command: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let page = self.page_handle(page_id).await?;
        if let Some(expected) = &command.expected_url {
            let current = page
                .url()
                .await
                .map_err(command_failed)?
                .unwrap_or_default();
            if &current != expected {
                return Err(driver_error(
                    ErrorCode::VerificationFailed,
                    format!("page URL is {current}, not the expected {expected}"),
                ));
            }
        }
        let resolved = self
            .resolve_target(page_id, &page, &command.selector, command.target.as_ref())
            .await?;
        let mut humanization_evidence = None;
        let observed = if resolved.is_checkable(&page).await? {
            let checked = command.value.parse::<bool>().map_err(|_| {
                driver_error(
                    ErrorCode::InvalidRequest,
                    "checkable controls require a boolean value",
                )
            })?;
            resolved.set_checked(&page, checked).await?.to_string()
        } else if resolved.is_select(&page).await? {
            resolved.select_option(&page, &command.value).await?
        } else {
            if self.humanization_enabled() {
                let synthesized = self
                    .humanized_type_text(&page, &resolved, &command.value, command.clear_first)
                    .await?;
                humanization_evidence = Some(Evidence::Humanization {
                    engine: "behavioral-engine".to_owned(),
                    actions: synthesized.0,
                    synthesized_ms: synthesized.1,
                });
            } else {
                resolved
                    .type_text(&page, &command.value, command.clear_first)
                    .await?;
            }
            resolved.value(&page).await?.unwrap_or_default()
        };
        let validity = resolved.form_control_validity(&page).await?;
        let mut evidence = vec![
            Evidence::Element {
                selector: command.selector.clone(),
                text: Some(observed),
            },
            Evidence::Configuration {
                name: "formControlValid".into(),
                value: validity.valid.to_string(),
            },
            Evidence::Configuration {
                name: "formControlValidationMessage".into(),
                value: validity.validation_message,
            },
            resolved.evidence,
        ];
        if let Some(humanization) = humanization_evidence {
            evidence.push(humanization);
        }
        Ok(evidence)
    }

    async fn upload_files(
        &self,
        page_id: &PageId,
        command: &UploadFilesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let requested = command.paths.iter().map(PathBuf::from).collect::<Vec<_>>();
        let paths = resolve_upload_paths(&self.upload_roots, &requested)?;
        let page = self.page_handle(page_id).await?;
        let resolved = self
            .resolve_target(page_id, &page, &command.selector, command.target.as_ref())
            .await?;
        let path_strings = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        resolved.set_files(&page, path_strings.clone()).await?;
        Ok(vec![
            Evidence::Upload {
                selector: command.selector.clone(),
                paths: path_strings,
            },
            resolved.evidence,
        ])
    }

    async fn control_action(
        &self,
        page_id: &PageId,
        command: &ControlActionCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        command
            .action
            .validate()
            .map_err(|message| driver_error(ErrorCode::InvalidRequest, message))?;
        let page = self.page_handle(page_id).await?;
        let read_snapshot = async || -> Result<types::FormSnapshot, CommandError> {
            let value: serde_json::Value = page
                .evaluate(crate::form_snapshot_expression(page_id))
                .await
                .map_err(command_failed)?
                .into_value()
                .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
            let encoded = value.as_str().ok_or_else(|| {
                driver_error(
                    ErrorCode::BrowserCommandFailed,
                    "form snapshot returned non-text JSON",
                )
            })?;
            crate::decode_form_snapshot(page_id.clone(), encoded, 512)
        };
        let find_control = |snapshot: &types::FormSnapshot| -> Option<FormControl> {
            snapshot
                .forms
                .iter()
                .flat_map(|form| form.controls.iter())
                .chain(snapshot.unowned_controls.iter())
                .find(|control| control.target.as_ref() == Some(&command.target))
                .cloned()
        };
        let before = read_snapshot().await?;
        let before_control = find_control(&before).ok_or_else(|| {
            driver_error(
                ErrorCode::TargetNotFound,
                "form control target was not found",
            )
        })?;
        crate::validate_control_action(&before_control, &command.action)?;

        let target = Self::control_target_spec(&command.target);
        let resolved = self
            .resolve_target(page_id, &page, "", Some(&target))
            .await?;
        match &command.action {
            ControlAction::SetText { value } => resolved.type_text(&page, value, true).await?,
            ControlAction::SetChecked { checked } => {
                resolved.set_checked(&page, *checked).await?;
            }
            ControlAction::SelectOne { value } => {
                resolved.select_option(&page, value).await?;
            }
            ControlAction::SelectMany { values } => {
                resolved.select_options(&page, values).await?;
            }
            ControlAction::SetFiles { paths } => {
                let requested = paths.iter().map(PathBuf::from).collect::<Vec<_>>();
                let paths = resolve_upload_paths(&self.upload_roots, &requested)?
                    .into_iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect();
                resolved.set_files(&page, paths).await?;
            }
            ControlAction::Clear => {
                if before_control.control_kind == types::FormControlKind::File {
                    resolved.set_files(&page, Vec::new()).await?;
                } else {
                    resolved.clear_control(&page).await?;
                }
            }
            ControlAction::Activate => resolved.click(&page).await?,
        }

        let after = read_snapshot().await?;
        let after_control = find_control(&after).ok_or_else(|| {
            driver_error(
                ErrorCode::TargetDetached,
                "form control was replaced or detached after dispatch",
            )
        })?;
        let evidence = crate::control_action_evidence(&after_control, &command.action, false)?;
        Ok(vec![Evidence::ControlAction { action: evidence }])
    }

    async fn open_page_command(
        &self,
        command: &OpenPageCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let page_id = PageId::new();
        let page = {
            let browser = self.browser.lock().await;
            let browser = browser.as_ref().ok_or_else(closed_error)?;
            // Always start blank so init scripts register before first real document.
            browser
                .new_page("about:blank")
                .await
                .map_err(command_failed)?
        };
        self.register_page(page_id.clone(), page).await?;
        if let Some(url) = command.url.as_deref() {
            let page = self.page_handle(&page_id).await?;
            page.goto(url).await.map_err(command_failed)?;
        }
        let page = self.page_handle(&page_id).await?;
        let evidence = page_evidence(page_id, &page).await?;
        Ok(vec![Evidence::Page {
            page_id: evidence.page_id,
            url: evidence.url,
            title: evidence.title,
        }])
    }

    async fn list_pages(&self, _command: &ListPagesCommand) -> Result<Vec<Evidence>, CommandError> {
        let handles: Vec<(PageId, Page)> = self
            .pages
            .lock()
            .await
            .iter()
            .map(|(page_id, page)| (page_id.clone(), page.clone()))
            .collect();
        let mut listed = Vec::with_capacity(handles.len());
        for (page_id, page) in handles {
            listed.push(page_evidence(page_id, &page).await?);
        }
        listed.sort_by_key(|page| page.page_id.0);
        Ok(vec![Evidence::Pages { pages: listed }])
    }

    async fn close_page_command(
        &self,
        command: &ClosePageCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let page = self
            .unregister_page(&command.page_id)
            .await
            .ok_or_else(page_missing)?;
        let evidence = page_evidence(command.page_id.clone(), &page).await?;
        page.close().await.map_err(command_failed)?;
        Ok(vec![Evidence::Page {
            page_id: evidence.page_id,
            url: evidence.url,
            title: evidence.title,
        }])
    }

    async fn network_log(
        &self,
        page_id: &PageId,
        command: &types::NetworkLogCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let recorder = self.ensure_har_collector(page_id).await?;
        let entries = recorder.take(command.clear).await;
        let page_url = match self.page_handle(page_id).await {
            Ok(page) => page.url().await.ok().flatten().unwrap_or_default(),
            Err(_) => String::new(),
        };
        let document = crate::har::har_document(&entries, &page_url);
        let bytes = serde_json::to_vec(&document)
            .map_err(|error| driver_error(ErrorCode::Internal, error.to_string()))?;
        let record = self
            .artifacts
            .put(
                &self.session_id,
                page_id,
                "application/json",
                "har",
                &bytes,
                MAX_VISION_SCREENSHOT_BYTES,
            )
            .await
            .map_err(|error| driver_error(ErrorCode::Internal, error))?;
        Ok(vec![Evidence::HarArtifact {
            artifact_id: record.artifact_id,
            media_type: record.media_type,
            bytes: record.bytes,
            sha256: record.sha256,
            entries: entries.len() as u32,
        }])
    }

    async fn emulate(
        &self,
        page_id: &PageId,
        command: &types::EmulateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let page = self.page_handle(page_id).await?;
        if let Some(viewport) = command.viewport {
            if viewport.width == 0
                || viewport.height == 0
                || viewport.width > 16384
                || viewport.height > 16384
            {
                return Err(driver_error(
                    ErrorCode::InvalidRequest,
                    "viewport dimensions must be within 1..=16384",
                ));
            }
            let params = chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::builder()
                .width(viewport.width as i64)
                .height(viewport.height as i64)
                .device_scale_factor(1.0)
                .mobile(command.mobile.unwrap_or(false))
                .build()
                .map_err(|error| driver_error(ErrorCode::InvalidRequest, error))?;
            bounded_cdp(page.execute(params), command_failed).await?;
        }
        if let Some(coordinates) = command.geolocation {
            if !coordinates.latitude.is_finite()
                || !coordinates.longitude.is_finite()
                || !(-90.0..=90.0).contains(&coordinates.latitude)
                || !(-180.0..=180.0).contains(&coordinates.longitude)
            {
                return Err(driver_error(
                    ErrorCode::InvalidRequest,
                    "geolocation coordinates are out of range",
                ));
            }
            let params = chromiumoxide::cdp::browser_protocol::emulation::SetGeolocationOverrideParams::builder()
                .latitude(coordinates.latitude)
                .longitude(coordinates.longitude)
                .accuracy(coordinates.accuracy.unwrap_or(1.0))
                .build();
            bounded_cdp(page.execute(params), command_failed).await?;
        }
        Ok(vec![Evidence::Emulation {
            viewport: command.viewport,
            geolocation: command.geolocation,
        }])
    }

    async fn handle_dialog(
        &self,
        page_id: &PageId,
        command: &types::HandleDialogCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let timeout = std::time::Duration::from_millis(
            command.timeout_ms.unwrap_or(30_000).clamp(1, 300_000),
        );
        let page = self.page_handle(page_id).await?;
        let mut events = page
            .event_listener::<chromiumoxide::cdp::browser_protocol::page::EventJavascriptDialogOpening>()
            .await
            .map_err(command_failed)?;
        let event = tokio::time::timeout(timeout, events.next())
            .await
            .map_err(|_| {
                driver_error(
                    ErrorCode::DeadlineExceeded,
                    format!(
                        "no JavaScript dialog opened within {}ms",
                        timeout.as_millis()
                    ),
                )
            })?
            .ok_or_else(|| driver_error(ErrorCode::NotFound, "dialog event stream closed"))?;
        let accept = matches!(command.action, types::DialogAction::Accept);
        page.execute(
            chromiumoxide::cdp::browser_protocol::page::HandleJavaScriptDialogParams::new(accept),
        )
        .await
        .map_err(command_failed)?;
        Ok(vec![Evidence::Dialog {
            dialog_type: format!("{:?}", event.r#type).to_lowercase(),
            message: event.message.clone(),
            action: if accept {
                "accept".into()
            } else {
                "dismiss".into()
            },
        }])
    }

    async fn print_to_pdf(
        &self,
        page_id: &PageId,
        command: &types::PrintToPdfCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let page = self.page_handle(page_id).await?;
        if let Some(scale) = command.scale {
            if !(0.1..=2.0).contains(&scale) {
                return Err(driver_error(
                    ErrorCode::InvalidRequest,
                    "PDF scale must be within 0.1..=2.0",
                ));
            }
        }
        let params = chromiumoxide::cdp::browser_protocol::page::PrintToPdfParams {
            landscape: Some(command.landscape),
            print_background: Some(command.print_background),
            scale: command.scale,
            page_ranges: command.page_ranges.clone(),
            ..Default::default()
        };
        let bytes = page.pdf(params).await.map_err(screenshot_error)?;
        let record = self
            .artifacts
            .put(
                &self.session_id,
                page_id,
                "application/pdf",
                "pdf",
                &bytes,
                MAX_VISION_SCREENSHOT_BYTES,
            )
            .await
            .map_err(|error| driver_error(ErrorCode::ScreenshotCaptureFailed, error))?;
        Ok(vec![Evidence::PdfArtifact {
            artifact_id: record.artifact_id,
            media_type: record.media_type,
            bytes: record.bytes,
            sha256: record.sha256,
        }])
    }

    async fn get_cookies(
        &self,
        page_id: &PageId,
        command: &types::GetCookiesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let page = self.page_handle(page_id).await?;
        let mut params = chromiumoxide::cdp::browser_protocol::network::GetCookiesParams::default();
        if !command.urls.is_empty() {
            params.urls = Some(command.urls.clone());
        }
        let result = bounded_cdp(page.execute(params), command_failed)
            .await?
            .result;
        Ok(vec![Evidence::CookieState {
            page_id: Some(page_id.clone()),
            cookies: result.cookies.into_iter().map(cookie_record).collect(),
        }])
    }

    async fn set_cookies(
        &self,
        page_id: &PageId,
        command: &types::SetCookiesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        // Clone the page handle and drop the pages guard before any browser
        // I/O: the read-back below locks the same (non-reentrant) mutex, so
        // holding it here deadlocks every cookie_set.
        let page = self.page_handle(page_id).await?;
        if command.cookies.len() > 128 {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                "cookie set exceeds the 128-cookie bound",
            ));
        }
        let params = chromiumoxide::cdp::browser_protocol::network::SetCookiesParams {
            cookies: command.cookies.iter().map(set_cookie_param).collect(),
        };
        bounded_cdp(page.execute(params), command_failed).await?;
        self.get_cookies(
            page_id,
            &types::GetCookiesCommand {
                urls: command
                    .cookies
                    .iter()
                    .map(|cookie| cookie.url.clone())
                    .collect(),
            },
        )
        .await
    }

    async fn delete_cookies(
        &self,
        page_id: &PageId,
        command: &types::DeleteCookiesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        // Same deadlock avoidance as set_cookies: take the handle, drop the
        // guard, then do I/O (both get_cookies calls lock pages again).
        let page = self.page_handle(page_id).await?;
        let current = self
            .get_cookies(
                page_id,
                &types::GetCookiesCommand {
                    urls: command.urls.clone(),
                },
            )
            .await?;
        let Some(Evidence::CookieState { cookies, .. }) = current.first() else {
            return Ok(current);
        };
        for cookie in cookies {
            if !command.names.is_empty() && !command.names.contains(&cookie.name) {
                continue;
            }
            let mut params =
                chromiumoxide::cdp::browser_protocol::network::DeleteCookiesParams::new(
                    &cookie.name,
                );
            params.url = command.urls.first().cloned().or_else(|| {
                Some(format!(
                    "https://{}{}",
                    cookie.domain.trim_start_matches('.'),
                    cookie.path
                ))
            });
            params.domain = Some(cookie.domain.clone());
            params.path = Some(cookie.path.clone());
            bounded_cdp(page.execute(params), command_failed).await?;
        }
        self.get_cookies(
            page_id,
            &types::GetCookiesCommand {
                urls: command.urls.clone(),
            },
        )
        .await
    }

    async fn screenshot_bytes(&self, page_id: &PageId) -> Result<Vec<u8>, CommandError> {
        let page = self.page_handle(page_id).await?;
        let bytes = page
            .screenshot(
                ScreenshotParams::builder()
                    .format(CaptureScreenshotFormat::Png)
                    .build(),
            )
            .await
            .map_err(screenshot_error)?;
        if bytes.len() > MAX_VISION_SCREENSHOT_BYTES {
            return Err(driver_error(
                ErrorCode::ScreenshotCaptureFailed,
                "screenshot exceeded the vision byte bound",
            ));
        }
        Ok(bytes)
    }

    async fn a11y_snapshot(
        &self,
        page_id: &PageId,
        command: &types::AccessibilitySnapshotCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let page = self.page_handle(page_id).await?;
        let result = page
            .execute(
                chromiumoxide::cdp::browser_protocol::accessibility::GetFullAxTreeParams::default(),
            )
            .await
            .map_err(command_failed)?
            .result;
        let max_nodes = command
            .max_nodes
            .unwrap_or(DEFAULT_A11Y_MAX_NODES)
            .clamp(1, MAX_A11Y_NODES) as usize;
        let (nodes, truncated) = compact_ax_tree(&result.nodes, max_nodes);
        Ok(vec![Evidence::AccessibilitySnapshot {
            page_id: page_id.clone(),
            nodes,
            truncated,
        }])
    }

    async fn form_snapshot(
        &self,
        page_id: &PageId,
        max_controls: Option<u32>,
    ) -> Result<Vec<Evidence>, CommandError> {
        let max_controls = max_controls.unwrap_or(512) as usize;
        let page = self.page_handle(page_id).await?;
        let value: serde_json::Value = page
            .evaluate(crate::form_snapshot_expression_with_limit(
                page_id,
                max_controls,
            ))
            .await
            .map_err(command_failed)?
            .into_value()
            .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
        let encoded = value.as_str().ok_or_else(|| {
            driver_error(
                ErrorCode::BrowserCommandFailed,
                "form snapshot returned non-text JSON",
            )
        })?;
        let snapshot = crate::decode_form_snapshot(page_id.clone(), encoded, max_controls)?;
        Ok(vec![Evidence::FormSnapshot { snapshot }])
    }

    async fn activate_page(
        &self,
        command: &types::ActivatePageCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let page = self.page_handle(&command.page_id).await?;
        page.activate().await.map_err(command_failed)?;
        let evidence = page_evidence(command.page_id.clone(), &page).await?;
        Ok(vec![Evidence::Page {
            page_id: evidence.page_id,
            url: evidence.url,
            title: evidence.title,
        }])
    }

    async fn click_and_wait_for_popup(
        &self,
        page_id: &PageId,
        command: &ClickAndWaitForPopupCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let opener = self
            .pages
            .lock()
            .await
            .get(page_id)
            .cloned()
            .ok_or_else(page_missing)?;
        let mut browser_guard = self.browser.lock().await;
        let browser = browser_guard.as_mut().ok_or_else(closed_error)?;
        let mut events = browser
            .event_listener::<EventTargetCreated>()
            .await
            .map_err(command_failed)?;
        let opener_target = opener.target_id().clone();
        let resolved = resolve_browser_target(
            page_id,
            &opener,
            &command.selector,
            command.target.as_ref(),
            Some(browser),
        )
        .await?;
        resolved.click(&opener).await?;
        let event = tokio::time::timeout(Duration::from_millis(command.timeout_ms), async {
            loop {
                let event = events.next().await.ok_or_else(|| {
                    driver_error(ErrorCode::BrowserCommandFailed, "popup event stream closed")
                })?;
                if event.target_info.opener_id.as_ref() == Some(&opener_target)
                    && event.target_info.r#type == "page"
                {
                    return Ok::<_, CommandError>(event);
                }
            }
        })
        .await
        .map_err(|_| timeout_error(command.timeout_ms))??;
        let popup = tokio::time::timeout(Duration::from_millis(command.timeout_ms), async {
            loop {
                if let Some(page) = browser
                    .pages()
                    .await
                    .map_err(command_failed)?
                    .into_iter()
                    .find(|page| page.target_id() == &event.target_info.target_id)
                {
                    return Ok::<_, CommandError>(page);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| timeout_error(command.timeout_ms))??;
        drop(browser_guard);
        let popup_id = PageId::new();
        let page = page_evidence(popup_id.clone(), &popup).await?;
        self.register_page(popup_id.clone(), popup).await?;
        Ok(vec![
            Evidence::Popup {
                opener_page_id: page_id.clone(),
                page_id: popup_id,
                url: page.url,
                title: page.title,
            },
            resolved.evidence,
        ])
    }
    async fn click_and_wait_for_download(
        &self,
        page_id: &PageId,
        command: &ClickAndWaitForDownloadCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let page = self.page_handle(page_id).await?;
        tokio::time::timeout(
            Duration::from_millis(command.timeout_ms),
            page.execute(
                SetDownloadBehaviorParams::builder()
                    .behavior(SetDownloadBehaviorBehavior::Allow)
                    .download_path(self.download_dir.to_string_lossy())
                    .events_enabled(true)
                    .build()
                    .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?,
            ),
        )
        .await
        .map_err(|_| timeout_error(command.timeout_ms))?
        .map_err(command_failed)?;
        let mut begins = page
            .event_listener::<EventDownloadWillBegin>()
            .await
            .map_err(command_failed)?;
        let mut progress = page
            .event_listener::<EventDownloadProgress>()
            .await
            .map_err(command_failed)?;
        let resolved = self
            .resolve_target(page_id, &page, &command.selector, command.target.as_ref())
            .await?;
        resolved.click_js(&page).await?;
        let begin = tokio::time::timeout(Duration::from_millis(command.timeout_ms), begins.next())
            .await
            .map_err(|_| timeout_error(command.timeout_ms))?
            .ok_or_else(|| {
                driver_error(
                    ErrorCode::BrowserCommandFailed,
                    "download event stream closed",
                )
            })?;
        let completed = tokio::time::timeout(Duration::from_millis(command.timeout_ms), async {
            loop {
                let event = progress.next().await.ok_or_else(|| {
                    driver_error(
                        ErrorCode::BrowserCommandFailed,
                        "download progress stream closed",
                    )
                })?;
                if event.guid == begin.guid {
                    match event.state {
                        DownloadProgressState::Completed => return Ok::<_, CommandError>(event),
                        DownloadProgressState::Canceled => {
                            return Err(driver_error(
                                ErrorCode::BrowserCommandFailed,
                                "download was canceled",
                            ))
                        }
                        DownloadProgressState::InProgress => {}
                    }
                }
            }
        })
        .await
        .map_err(|_| timeout_error(command.timeout_ms))??;
        let path = completed
            .file_path
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.download_dir.join(&begin.suggested_filename));
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
        Ok(vec![
            Evidence::Download {
                filename: begin.suggested_filename.clone(),
                path: path.to_string_lossy().into_owned(),
                bytes: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&bytes)),
            },
            resolved.evidence,
        ])
    }

    async fn wait_for(
        &self,
        page_id: &PageId,
        command: &WaitForCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let started = Instant::now();
        let deadline = started + Duration::from_millis(command.timeout_ms);
        let mut observations = 0;
        let mut quiet_since = None;
        loop {
            observations += 1;
            let tracker = self.network_trackers.lock().await.get(page_id).cloned();
            let page = self.page_handle(page_id).await?;
            let (satisfied, excluded_classes) = wait_condition_satisfied(
                &self.browser,
                page_id,
                &page,
                tracker.as_deref(),
                &command.condition,
                &mut quiet_since,
            )
            .await?;
            if satisfied {
                return Ok(vec![Evidence::Wait {
                    condition: command.condition.clone(),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    observations,
                    excluded_classes,
                }]);
            }
            if Instant::now() >= deadline {
                return Err(CommandError {
                    code: ErrorCode::WaitConditionTimedOut,
                    message: format!(
                        "wait condition was not satisfied within {}ms",
                        command.timeout_ms
                    ),
                    layer: ErrorLayer::Driver,
                    retryable: false,
                });
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn collect_candidates(
        &self,
        page_id: &PageId,
        target: &types::TargetSpec,
    ) -> Result<Vec<dom_engine::Candidate>, CommandError> {
        let page = self.page_handle(page_id).await?;
        let mut browser = self.browser.lock().await;
        let browser = browser.as_mut().ok_or_else(closed_error)?;
        gather_candidates(&page, target, Some(browser)).await
    }

    async fn capture_screenshot(
        &self,
        page_id: &PageId,
        command: &CaptureScreenshotCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let page = self.page_handle(page_id).await?;
        let (bytes, resolution) = match &command.mode {
            ScreenshotMode::Viewport => (
                bounded_cdp(
                    page.screenshot(
                        ScreenshotParams::builder()
                            .format(CaptureScreenshotFormat::Png)
                            .build(),
                    ),
                    screenshot_error,
                )
                .await?,
                None,
            ),
            ScreenshotMode::FullPage => (
                bounded_cdp(
                    page.screenshot(
                        ScreenshotParams::builder()
                            .format(CaptureScreenshotFormat::Png)
                            .full_page(true)
                            .build(),
                    ),
                    screenshot_error,
                )
                .await?,
                None,
            ),
            ScreenshotMode::Element { target } => {
                let resolved = self
                    .resolve_target(page_id, &page, "", Some(target))
                    .await?;
                let bytes = bounded_cmd(resolved.screenshot(&page)).await?;
                (bytes, Some(resolved.evidence))
            }
            ScreenshotMode::Clip {
                x,
                y,
                width,
                height,
            } => {
                if !x.is_finite()
                    || !y.is_finite()
                    || !width.is_finite()
                    || !height.is_finite()
                    || *width <= 0.0
                    || *height <= 0.0
                {
                    return Err(driver_error(
                        ErrorCode::InvalidRequest,
                        "screenshot clip must have finite positive dimensions",
                    ));
                }
                (
                    bounded_cdp(
                        page.screenshot(
                            ScreenshotParams::builder()
                                .format(CaptureScreenshotFormat::Png)
                                .clip(Viewport {
                                    x: *x,
                                    y: *y,
                                    width: *width,
                                    height: *height,
                                    scale: 1.0,
                                })
                                .build(),
                        ),
                        screenshot_error,
                    )
                    .await?,
                    None,
                )
            }
        };
        let record = self
            .artifacts
            .put_png(&self.session_id, page_id, &bytes)
            .await
            .map_err(|error| driver_error(ErrorCode::ScreenshotCaptureFailed, error))?;
        let mut evidence = vec![Evidence::Screenshot {
            artifact_id: record.artifact_id,
            media_type: record.media_type,
            width: record.width,
            height: record.height,
            bytes: record.bytes,
            sha256: record.sha256,
        }];
        if let Some(resolution) = resolution {
            evidence.push(resolution);
        }
        Ok(evidence)
    }

    async fn set_focus_emulation(
        &self,
        page_id: &PageId,
        command: &SetFocusEmulationCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let page = self.page_handle(page_id).await?;
        page.execute(SetFocusEmulationEnabledParams::new(command.enabled))
            .await
            .map_err(command_failed)?;
        Ok(vec![Evidence::Configuration {
            name: "focusEmulation".into(),
            value: command.enabled.to_string(),
        }])
    }

    async fn set_emulated_media(
        &self,
        page_id: &PageId,
        command: &SetEmulatedMediaCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let page = self.page_handle(page_id).await?;
        let features = command
            .features
            .iter()
            .map(|(name, value)| MediaFeature::new(name, value));
        page.execute(
            SetEmulatedMediaParams::builder()
                .media(&command.media)
                .features(features)
                .build(),
        )
        .await
        .map_err(command_failed)?;
        Ok(vec![Evidence::Configuration {
            name: "emulatedMedia".into(),
            value: serde_json::to_string(&command)
                .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?,
        }])
    }

    // SECURITY(F4): the two authoritative deny-by-default gates for JS
    // evaluation, the token capability check (`AuthenticatedRuntime::submit`)
    // and the per-session `ExecutionPolicy` check (`RuntimeService::submit`),
    // are enforced upstream; both land before `execute()` reaches a
    // `BrowserWorker`. There is deliberately no worker-level backstop:
    // `WorkerFactory::launch(&SessionId)` gives this worker no access to the
    // session's `ExecutionPolicy`.
    async fn evaluate_javascript(
        &self,
        page_id: &PageId,
        command: &EvaluateJavaScriptCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let page = self.page_handle(page_id).await?;

        let mut params = EvaluateParams::new(command.expression.clone());
        params.await_promise = Some(command.await_promise);
        params.return_by_value = Some(true);

        // DoS clamp: a caller-supplied `timeout_ms` is otherwise unbounded and could
        // pin a worker lease open arbitrarily long. Clamp to the configured ceiling
        // rather than rejecting, so the command still runs under the common bound.
        let timeout_ms = clamp_js_timeout_ms(command.timeout_ms, self.max_js_timeout_ms);

        let value: serde_json::Value =
            tokio::time::timeout(Duration::from_millis(timeout_ms), page.evaluate(params))
                .await
                .map_err(|_| timeout_error(timeout_ms))?
                .map_err(command_failed)?
                .into_value()
                .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;

        let (value, truncated) = js_engine::bound_result(value, self.max_js_result_bytes);
        Ok(vec![Evidence::JavaScriptResult { value, truncated }])
    }

    fn supports_http_state(&self) -> bool {
        true
    }

    async fn http_state(&self, page_id: &PageId) -> Result<HttpStateSnapshot, CommandError> {
        let page = self.page_handle(page_id).await?;
        let state = self.http_state.lock().await;
        let current_url = page
            .url()
            .await
            .map_err(command_failed)?
            .unwrap_or_default();
        let cookies = page
            .get_cookies()
            .await
            .map_err(command_failed)?
            .into_iter()
            .map(snapshot_cookie)
            .collect::<Result<Vec<_>, _>>()?;
        let user_agent = page
            .evaluate("navigator.userAgent")
            .await
            .map_err(command_failed)?
            .into_value()
            .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
        let language = page
            .evaluate("navigator.language")
            .await
            .map_err(command_failed)?
            .into_value()
            .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
        Ok(HttpStateSnapshot {
            version: state.version,
            current_url,
            cookies,
            cache_validators: state.cache_validators.clone(),
            user_agent,
            language,
        })
    }

    async fn commit_http_state(
        &self,
        page_id: &PageId,
        expected_version: u64,
        delta: ResponseStateDelta,
    ) -> Result<(), CommandError> {
        let page = self.page_handle(page_id).await?;
        let current_url = bounded_cdp(page.url(), command_failed)
            .await?
            .ok_or_else(|| driver_error(ErrorCode::InvalidRequest, "page URL is unavailable"))?;
        let parsed_url = url::Url::parse(&current_url)
            .map_err(|error| driver_error(ErrorCode::InvalidRequest, error))?;
        validate_state_delta(&delta, &parsed_url)?;
        let mut state = self.http_state.lock().await;
        if state.version != expected_version {
            return Err(http_state_conflict(expected_version, state.version));
        }
        let next_version = state.version.checked_add(1).ok_or_else(|| {
            driver_error(
                ErrorCode::BrowserCommandFailed,
                "HTTP state version exhausted",
            )
        })?;
        if delta.cookies.is_empty() {
            finish_state_commit(&mut state, next_version, delta.cache_validators);
            return Ok(());
        }
        let cookies = delta
            .cookies
            .into_iter()
            .map(|cookie| cookie_param(cookie, &current_url))
            .collect::<Result<Vec<_>, _>>()?;
        apply_state_commit(&mut state, next_version, delta.cache_validators, async {
            page.execute(SetCookiesParams::new(cookies))
                .await
                .map(|_| ())
        })
        .await
        .map_err(command_failed)?;
        Ok(())
    }

    async fn close(&self) -> Result<(), CommandError> {
        self.pages.lock().await.clear();
        self.network_trackers.lock().await.clear();
        if let Some(mut browser) = self.browser.lock().await.take() {
            browser.close().await.map_err(command_failed)?;
        }
        if let Some(task) = self.handler_task.lock().await.take() {
            task.abort();
        }
        if let Some(path) = &self.pid_registry_path {
            unregister_chrome_pid(path);
        }
        Ok(())
    }

    async fn terminate(&self) -> Result<(), CommandError> {
        self.pages.lock().await.clear();
        self.network_trackers.lock().await.clear();
        let close_result = if let Some(mut browser) = self.browser.lock().await.take() {
            browser.close().await.map(|_| ()).map_err(command_failed)
        } else {
            Ok(())
        };
        if let Some(task) = self.handler_task.lock().await.take() {
            task.abort();
        }
        if let Some(path) = &self.pid_registry_path {
            unregister_chrome_pid(path);
        }
        close_result
    }
}

fn validate_state_delta(
    delta: &ResponseStateDelta,
    current_url: &url::Url,
) -> Result<(), CommandError> {
    let host = current_url.host_str().ok_or_else(|| {
        driver_error(
            ErrorCode::InvalidRequest,
            "page URL does not have a cookie host",
        )
    })?;
    if !matches!(current_url.scheme(), "http" | "https") {
        return Err(driver_error(
            ErrorCode::InvalidRequest,
            "page URL scheme cannot carry HTTP cookies",
        ));
    }
    for cookie in &delta.cookies {
        if cookie.name.is_empty()
            || cookie
                .name
                .bytes()
                .any(|byte| byte <= 0x20 || byte >= 0x7f || b"()<>@,;:\\\"/[]?={}".contains(&byte))
            || cookie.value.contains(['\r', '\n', '\0'])
            || cookie.path.is_empty()
            || !cookie.path.starts_with('/')
        {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                "invalid HTTP cookie",
            ));
        }
        let domain = cookie.domain.strip_prefix('.').unwrap_or(&cookie.domain);
        if !domain.is_empty() {
            let parsed_domain = url::Host::parse(domain)
                .map_err(|_| driver_error(ErrorCode::InvalidRequest, "cookie domain is invalid"))?;
            let normalized_domain = parsed_domain.to_string();
            let in_scope = if matches!(parsed_domain, url::Host::Domain(_)) {
                host.eq_ignore_ascii_case(&normalized_domain)
                    || host
                        .to_ascii_lowercase()
                        .strip_suffix(&normalized_domain)
                        .is_some_and(|prefix| prefix.ends_with('.'))
            } else {
                host.eq_ignore_ascii_case(&normalized_domain)
            };
            if !in_scope {
                return Err(driver_error(
                    ErrorCode::InvalidRequest,
                    "cookie domain is outside the current page scope",
                ));
            }
        }
        if cookie.secure && current_url.scheme() != "https" {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                "secure cookie cannot be set from a non-HTTPS page",
            ));
        }
        if cookie
            .same_site
            .as_deref()
            .is_some_and(|same_site| same_site.eq_ignore_ascii_case("none"))
            && !cookie.secure
        {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                "SameSite=None cookie must be secure",
            ));
        }
        if cookie
            .expires_unix
            .is_some_and(|expiry| !expiry.is_finite() || expiry <= 0.0)
        {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                "cookie expiry must be a finite positive Unix timestamp",
            ));
        }
        let expected_source_scheme = if current_url.scheme() == "https" {
            "Secure"
        } else {
            "NonSecure"
        };
        if cookie
            .source_scheme
            .as_deref()
            .is_some_and(|scheme| !scheme.eq_ignore_ascii_case(expected_source_scheme))
        {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                "cookie source scheme does not match the current page",
            ));
        }
        if cookie.source_port.is_some_and(|port| {
            port == -1
                || !(1..=65_535).contains(&port)
                || current_url.port_or_known_default().map(i64::from) != Some(port)
        }) {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                "cookie source port does not match the current page",
            ));
        }
        if let Some(key) = &cookie.partition_key {
            let site = url::Url::parse(&key.top_level_site).map_err(|_| {
                driver_error(
                    ErrorCode::InvalidRequest,
                    "cookie partition site is invalid",
                )
            })?;
            if !matches!(site.scheme(), "http" | "https") || site.host_str().is_none() {
                return Err(driver_error(
                    ErrorCode::InvalidRequest,
                    "cookie partition site is invalid",
                ));
            }
        }
    }
    Ok(())
}

fn snapshot_cookie(cookie: Cookie) -> Result<HttpCookie, CommandError> {
    if cookie.partition_key_opaque == Some(true) {
        return Err(http_equivalence_unproven(
            "opaque partitioned cookie cannot be represented",
        ));
    }
    if !cookie.session && !cookie.expires.is_finite() {
        return Err(http_equivalence_unproven(
            "cookie expiry cannot be represented",
        ));
    }
    let host_only = !cookie.domain.starts_with('.');
    Ok(HttpCookie {
        name: cookie.name,
        value: cookie.value,
        domain: cookie.domain,
        host_only,
        path: cookie.path,
        secure: cookie.secure,
        http_only: cookie.http_only,
        same_site: cookie.same_site.map(|value| value.as_ref().to_owned()),
        expires_unix: (!cookie.session).then_some(cookie.expires),
        priority: Some(cookie.priority.as_ref().to_owned()),
        source_scheme: Some(cookie.source_scheme.as_ref().to_owned()),
        source_port: Some(cookie.source_port),
        partition_key: cookie.partition_key.map(|key| HttpCookiePartitionKey {
            top_level_site: key.top_level_site,
            has_cross_site_ancestor: key.has_cross_site_ancestor,
        }),
    })
}

fn cookie_param(cookie: HttpCookie, current_url: &str) -> Result<CookieParam, CommandError> {
    let same_site = cookie
        .same_site
        .map(|value| value.parse::<CookieSameSite>())
        .transpose()
        .map_err(|_| driver_error(ErrorCode::InvalidRequest, "invalid cookie SameSite value"))?;
    let mut param = CookieParam::new(cookie.name, cookie.value);
    param.url = Some(current_url.to_owned());
    param.domain = (!cookie.host_only && !cookie.domain.is_empty()).then_some(cookie.domain);
    param.path = (!cookie.path.is_empty()).then_some(cookie.path);
    param.secure = Some(cookie.secure);
    param.http_only = Some(cookie.http_only);
    param.same_site = same_site;
    param.expires = cookie.expires_unix.map(TimeSinceEpoch::new);
    param.priority = cookie
        .priority
        .map(|value| value.parse::<CookiePriority>())
        .transpose()
        .map_err(|_| driver_error(ErrorCode::InvalidRequest, "invalid cookie priority"))?;
    param.source_scheme = cookie
        .source_scheme
        .map(|value| value.parse::<CookieSourceScheme>())
        .transpose()
        .map_err(|_| driver_error(ErrorCode::InvalidRequest, "invalid cookie source scheme"))?;
    param.source_port = cookie.source_port;
    param.partition_key = cookie
        .partition_key
        .map(|key| CookiePartitionKey::new(key.top_level_site, key.has_cross_site_ancestor));
    Ok(param)
}

fn finish_state_commit(
    state: &mut HttpBridgeState,
    next_version: u64,
    validators: BTreeMap<String, String>,
) {
    state.cache_validators.extend(validators);
    state.version = next_version;
}

async fn apply_state_commit<E>(
    state: &mut HttpBridgeState,
    next_version: u64,
    validators: BTreeMap<String, String>,
    application: impl std::future::Future<Output = Result<(), E>>,
) -> Result<(), E> {
    application.await?;
    finish_state_commit(state, next_version, validators);
    Ok(())
}

fn http_state_conflict(expected: u64, actual: u64) -> CommandError {
    CommandError {
        code: ErrorCode::HttpStateConflict,
        message: format!("HTTP state version conflict: expected {expected}, current {actual}"),
        layer: ErrorLayer::Driver,
        retryable: false,
    }
}

fn http_equivalence_unproven(message: impl Into<String>) -> CommandError {
    CommandError {
        code: ErrorCode::HttpEquivalenceUnproven,
        message: message.into(),
        layer: ErrorLayer::Driver,
        retryable: false,
    }
}

async fn wait_condition_satisfied(
    browser: &Mutex<Option<Browser>>,
    page_id: &PageId,
    page: &Page,
    tracker: Option<&crate::network_quiet::NetworkQuietTracker>,
    condition: &WaitCondition,
    quiet_since: &mut Option<Instant>,
) -> Result<(bool, Vec<String>), CommandError> {
    match condition {
        WaitCondition::Element { target, state } => {
            let mut browser = browser.lock().await;
            let resolved = match browser.as_mut() {
                Some(browser) => {
                    resolve_target_with_visibility(
                        page_id,
                        page,
                        "",
                        Some(target),
                        false,
                        Some(browser),
                    )
                    .await
                }
                None => Err(closed_error()),
            };
            let resolved = match resolved {
                Ok(resolved) => Some(resolved),
                Err(error) if matches!(error.code, ErrorCode::TargetNotFound) => None,
                Err(error) => return Err(error),
            };
            let Some(resolved) = resolved else {
                return Ok((matches!(state, types::ElementState::Detached), Vec::new()));
            };
            let visible = resolved.visible(page).await?;
            let enabled = resolved.enabled(page).await?;
            Ok((
                match state {
                    types::ElementState::Attached => true,
                    types::ElementState::Detached => false,
                    types::ElementState::Visible => visible,
                    types::ElementState::Hidden => !visible,
                    types::ElementState::Enabled => enabled,
                    types::ElementState::Disabled => !enabled,
                },
                Vec::new(),
            ))
        }
        WaitCondition::Text { target, matcher } | WaitCondition::Value { target, matcher } => {
            let mut browser = browser.lock().await;
            let resolved = match browser.as_mut() {
                Some(browser) => {
                    resolve_browser_target(page_id, page, "", Some(target), Some(browser)).await
                }
                None => Err(closed_error()),
            };
            let resolved = match resolved {
                Ok(resolved) => resolved,
                Err(error) if matches!(error.code, ErrorCode::TargetNotFound) => {
                    return Ok((false, Vec::new()))
                }
                Err(error) => return Err(error),
            };
            let value = if matches!(condition, WaitCondition::Value { .. }) {
                resolved.value(page).await?.unwrap_or_default()
            } else {
                resolved.inner_text(page).await?.unwrap_or_default()
            };
            Ok((text_matches(matcher, &value)?, Vec::new()))
        }
        WaitCondition::Url { matcher } => {
            let url = page
                .url()
                .await
                .map_err(command_failed)?
                .unwrap_or_default();
            Ok((text_matches(matcher, &url)?, Vec::new()))
        }
        WaitCondition::Document { ready } => {
            let state: String = page
                .evaluate("document.readyState")
                .await
                .map_err(command_failed)?
                .into_value()
                .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
            Ok((
                match ready {
                    WaitUntil::Commit => true,
                    WaitUntil::DomContentLoaded | WaitUntil::Interactive => {
                        state == "interactive" || state == "complete"
                    }
                    WaitUntil::NetworkIdle => state == "complete",
                },
                Vec::new(),
            ))
        }
        WaitCondition::NetworkQuiet {
            idle_ms,
            max_in_flight,
            ignore_url_substrings,
            ignore_resource_types,
            ignore_long_lived,
        } => {
            let tracker = tracker.ok_or_else(|| {
                driver_error(
                    ErrorCode::BrowserCommandFailed,
                    "network quiet tracker is not attached to this page",
                )
            })?;
            let filters = crate::network_quiet::NetworkQuietFilters {
                ignore_url_substrings,
                ignore_resource_types,
                ignore_long_lived: *ignore_long_lived,
            };
            let (in_flight, excluded_classes) = tracker.snapshot(&filters).await;
            if in_flight <= *max_in_flight {
                let since = quiet_since.get_or_insert_with(Instant::now);
                Ok((
                    since.elapsed() >= Duration::from_millis(*idle_ms),
                    excluded_classes,
                ))
            } else {
                *quiet_since = None;
                Ok((false, excluded_classes))
            }
        }
    }
}

fn text_matches(matcher: &types::TextMatch, value: &str) -> Result<bool, CommandError> {
    match matcher {
        types::TextMatch::Exact(expected) => Ok(value == expected),
        types::TextMatch::Contains(expected) => Ok(value.contains(expected)),
        types::TextMatch::Regex(pattern) => {
            if pattern.len() > 256 {
                return Err(driver_error(
                    ErrorCode::InvalidRequest,
                    "wait regular expression exceeds configured limit",
                ));
            }
            regex::Regex::new(pattern)
                .map(|regex| regex.is_match(value))
                .map_err(|error| driver_error(ErrorCode::InvalidRequest, error))
        }
    }
}

async fn page_evidence(page_id: PageId, page: &Page) -> Result<PageEvidence, CommandError> {
    Ok(PageEvidence {
        page_id,
        url: page
            .url()
            .await
            .map_err(command_failed)?
            .unwrap_or_default(),
        title: page
            .get_title()
            .await
            .map_err(command_failed)?
            .unwrap_or_default(),
    })
}

/// Upper bound on a single CDP call that carries no command timeout of its
/// own: a hung browser must fail the command at 30s, not park it for the
/// whole envelope deadline. The envelope deadline (executor) still bounds
/// the command as a whole; this bounds each browser round trip inside it.
const DEFAULT_CDP_CALL_TIMEOUT: Duration = Duration::from_secs(30);

fn cdp_deadline_error() -> CommandError {
    CommandError {
        code: ErrorCode::DeadlineExceeded,
        message: format!(
            "browser did not answer within {}ms",
            DEFAULT_CDP_CALL_TIMEOUT.as_millis()
        ),
        layer: ErrorLayer::Driver,
        retryable: true,
    }
}

async fn bounded_cdp<T, E, F>(
    future: impl std::future::Future<Output = Result<T, E>>,
    map: F,
) -> Result<T, CommandError>
where
    E: std::fmt::Display,
    F: FnOnce(E) -> CommandError,
{
    match tokio::time::timeout(DEFAULT_CDP_CALL_TIMEOUT, future).await {
        Ok(result) => result.map_err(map),
        Err(_) => Err(cdp_deadline_error()),
    }
}

/// Same bound for calls that already return CommandError.
async fn bounded_cmd<T>(
    future: impl std::future::Future<Output = Result<T, CommandError>>,
) -> Result<T, CommandError> {
    match tokio::time::timeout(DEFAULT_CDP_CALL_TIMEOUT, future).await {
        Ok(result) => result,
        Err(_) => Err(cdp_deadline_error()),
    }
}

fn command_failed(error: chromiumoxide::error::CdpError) -> CommandError {
    driver_error(ErrorCode::BrowserCommandFailed, error)
}

fn screenshot_error(error: chromiumoxide::error::CdpError) -> CommandError {
    driver_error(ErrorCode::ScreenshotCaptureFailed, error)
}

fn driver_error(code: ErrorCode, error: impl std::fmt::Display) -> CommandError {
    CommandError {
        code,
        message: error.to_string(),
        layer: ErrorLayer::Driver,
        retryable: true,
    }
}

fn page_missing() -> CommandError {
    CommandError {
        code: ErrorCode::NotFound,
        message: "browser page is not open".into(),
        layer: ErrorLayer::Driver,
        retryable: false,
    }
}

fn closed_error() -> CommandError {
    CommandError {
        code: ErrorCode::BrowserCommandFailed,
        message: "browser worker is closed".into(),
        layer: ErrorLayer::Driver,
        retryable: false,
    }
}

/// DoS clamp for `EvaluateJavaScript::timeout_ms`: bounds a caller-requested
/// timeout to the configured `max_js_timeout_ms` ceiling so no caller can pin a
/// worker lease open indefinitely.
fn clamp_js_timeout_ms(requested_ms: u64, max_ms: u64) -> u64 {
    requested_ms.min(max_ms)
}

fn timeout_error(timeout_ms: u64) -> CommandError {
    CommandError {
        code: ErrorCode::DeadlineExceeded,
        message: format!("browser command exceeded {timeout_ms}ms"),
        layer: ErrorLayer::Driver,
        retryable: true,
    }
}

const MAX_VISION_SCREENSHOT_BYTES: usize = 16 * 1024 * 1024;

const DEFAULT_A11Y_MAX_NODES: u32 = 256;
const MAX_A11Y_NODES: u32 = 2048;

/// Collapses Chrome's flat AXNode list into the engine-shared compact tree.
/// Nodes Chrome marks ignored are skipped (their children are re-parented
/// upward); generic containers without names are kept only as structure.
fn compact_ax_tree(
    raw: &[chromiumoxide::cdp::browser_protocol::accessibility::AxNode],
    max_nodes: usize,
) -> (Vec<types::AccessibilityNode>, bool) {
    use chromiumoxide::cdp::browser_protocol::accessibility::AxNode;
    use std::collections::HashMap;

    fn text(
        value: &Option<chromiumoxide::cdp::browser_protocol::accessibility::AxValue>,
    ) -> Option<String> {
        value
            .as_ref()
            .and_then(|value| value.value.as_ref())
            .and_then(|value| value.as_str())
            .map(str::to_owned)
            .filter(|value| !value.is_empty())
    }

    fn property_text(node: &AxNode, name: &str) -> Option<String> {
        node.properties
            .as_ref()?
            .iter()
            .find(|property| property.name.as_ref() == name)?
            .value
            .value
            .as_ref()
            .and_then(|value| match value {
                serde_json::Value::String(value) => Some(value.clone()),
                serde_json::Value::Bool(value) => Some(value.to_string()),
                serde_json::Value::Number(value) => Some(value.to_string()),
                _ => None,
            })
    }

    fn property_bool(node: &AxNode, name: &str) -> Option<bool> {
        property_text(node, name).and_then(|value| match value.as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        })
    }

    let mut target_totals = std::collections::BTreeMap::new();
    for node in raw {
        let role = text(&node.role);
        let name = text(&node.name);
        if let (Some(role), Some(name)) = (role, name) {
            if !node.ignored
                && super::accessibility_role_is_actionable(&role)
                && !name.is_empty()
                && name != "[redacted]"
            {
                *target_totals.entry((role, name)).or_default() += 1;
            }
        }
    }

    let by_id: HashMap<&str, &AxNode> = raw
        .iter()
        .map(|node| (node.node_id.as_ref(), node))
        .collect();
    let mut budget = max_nodes;
    fn build(
        id: &str,
        by_id: &HashMap<&str, &AxNode>,
        budget: &mut usize,
        depth: usize,
    ) -> Option<types::AccessibilityNode> {
        let node = by_id.get(id)?;
        let mut children = Vec::new();
        if depth < 64 {
            if let Some(child_ids) = &node.child_ids {
                for child_id in child_ids {
                    if let Some(mut child) = build(child_id.as_ref(), by_id, budget, depth + 1) {
                        if child.role.is_none() && child.name.is_none() {
                            children.append(&mut child.children);
                        } else {
                            children.push(child);
                        }
                    }
                }
            }
        }
        if node.ignored {
            return (!children.is_empty()).then_some(types::AccessibilityNode {
                children,
                ..types::AccessibilityNode::default()
            });
        }
        let role = text(&node.role);
        let name = text(&node.name);
        let autocomplete = property_text(node, "autocomplete");
        let raw_value = text(&node.value);
        let masked_value = raw_value.as_deref().is_some_and(|value| {
            !value.is_empty()
                && value
                    .chars()
                    .all(|character| matches!(character, '•' | '●' | '*' | '◦'))
        });
        let password_control = autocomplete
            .as_deref()
            .is_some_and(|value| value.to_ascii_lowercase().contains("password"));
        let value = if masked_value || password_control {
            raw_value.map(|_| "[redacted]".to_owned())
        } else {
            raw_value
        };
        // Skip unlabeled generic wrappers; keep their children by re-parenting.
        if matches!(
            role.as_deref(),
            None | Some("generic" | "InlineTextBox" | "none")
        ) && name.is_none()
        {
            return (!children.is_empty()).then_some(types::AccessibilityNode {
                children,
                ..types::AccessibilityNode::default()
            });
        }
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        Some(types::AccessibilityNode {
            role,
            name,
            target: None,
            value,
            description: text(&node.description),
            required: property_bool(node, "required"),
            disabled: property_bool(node, "disabled"),
            read_only: property_bool(node, "readonly"),
            invalid: property_text(node, "invalid").map(|value| value != "false"),
            checked: property_bool(node, "checked"),
            autocomplete,
            value_min: property_text(node, "valuemin"),
            value_max: property_text(node, "valuemax"),
            children,
        })
    }

    fn lift(node: types::AccessibilityNode, lifted: &mut Vec<types::AccessibilityNode>) {
        lifted.push(node);
    }

    let roots: Vec<&str> = raw
        .iter()
        .filter(|node| {
            node.parent_id
                .as_ref()
                .is_none_or(|parent| !by_id.contains_key(parent.as_ref()))
        })
        .map(|node| node.node_id.as_ref())
        .collect();
    let mut roots_built: Vec<types::AccessibilityNode> = Vec::new();
    for root in &roots {
        if let Some(mut node) = build(root, &by_id, &mut budget, 0) {
            // Re-parent children of skipped nodes upward.
            if node.role.is_none() && node.name.is_none() {
                roots_built.append(&mut node.children);
            } else {
                lift(node, &mut roots_built);
            }
        }
    }
    let truncated = budget == 0 && raw.len() > max_nodes;
    super::annotate_accessibility_targets_with_totals(&mut roots_built, &target_totals);
    (roots_built, truncated)
}

fn cookie_record(
    cookie: chromiumoxide::cdp::browser_protocol::network::Cookie,
) -> types::CookieRecord {
    types::CookieRecord {
        name: cookie.name,
        value: cookie.value,
        domain: cookie.domain,
        path: cookie.path,
        secure: cookie.secure,
        http_only: cookie.http_only,
        same_site: cookie.same_site.map(|value| format!("{value:?}")),
        expires_unix: Some(cookie.expires),
    }
}

fn set_cookie_param(
    param: &types::SetCookieParam,
) -> chromiumoxide::cdp::browser_protocol::network::CookieParam {
    use chromiumoxide::cdp::browser_protocol::network::{
        CookieParam, CookieSameSite, TimeSinceEpoch,
    };
    let mut built = CookieParam::new(&param.name, &param.value);
    built.url = Some(param.url.clone());
    built.path = param.path.clone();
    built.secure = Some(param.secure);
    built.http_only = Some(param.http_only);
    built.same_site = param.same_site.as_deref().and_then(|value| match value {
        "Strict" | "strict" => Some(CookieSameSite::Strict),
        "Lax" | "lax" => Some(CookieSameSite::Lax),
        "None" | "none" => Some(CookieSameSite::None),
        _ => None,
    });
    built.expires = param.expires_unix.map(TimeSinceEpoch::new);
    built
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use chromiumoxide::cdp::browser_protocol::network::{
        Cookie, CookiePriority, CookieSourceScheme,
    };

    use super::{
        apply_state_commit, clamp_js_timeout_ms, compact_ax_tree, snapshot_cookie, text_matches,
        HttpBridgeState,
    };
    use types::{ErrorCode, TextMatch};

    #[test]
    fn accessibility_snapshot_preserves_form_state_and_redacts_masked_values() {
        let raw: Vec<chromiumoxide::cdp::browser_protocol::accessibility::AxNode> = serde_json::from_value(serde_json::json!([{
            "nodeId": "root",
            "ignored": true,
            "childIds": ["1", "2"]
        }, {
            "nodeId": "1",
            "ignored": false,
            "parentId": "root",
            "role": {"type": "role", "value": "textbox"},
            "name": {"type": "computedString", "value": "Password"},
            "description": {"type": "computedString", "value": "At least eight characters"},
            "value": {"type": "string", "value": "••••••••"},
            "properties": [
                {"name": "required", "value": {"type": "boolean", "value": true}},
                {"name": "invalid", "value": {"type": "token", "value": "true"}},
                {"name": "readonly", "value": {"type": "boolean", "value": false}},
                {"name": "autocomplete", "value": {"type": "token", "value": "current-password"}}
            ]
        }, {
            "nodeId": "2",
            "ignored": false,
            "parentId": "root",
            "role": {"type": "role", "value": "textbox"},
            "name": {"type": "computedString", "value": "Password"},
            "value": {"type": "string", "value": "••••"}
        }])).expect("valid CDP AX fixture");

        let (nodes, truncated) = compact_ax_tree(&raw, 10);
        assert!(!truncated);
        assert_eq!(nodes[0].value.as_deref(), Some("[redacted]"));
        assert_eq!(
            nodes[0].description.as_deref(),
            Some("At least eight characters")
        );
        assert_eq!(nodes[0].required, Some(true));
        assert_eq!(nodes[0].invalid, Some(true));
        assert_eq!(nodes[0].read_only, Some(false));
        assert_eq!(nodes[0].autocomplete.as_deref(), Some("current-password"));
        assert_eq!(nodes[0].target.as_ref().unwrap().role, "textbox");
        assert_eq!(
            nodes[0].target.as_ref().unwrap().accessible_name,
            "Password"
        );
        assert_eq!(nodes[0].target.as_ref().unwrap().ordinal, Some(0));
        assert_eq!(nodes[1].target.as_ref().unwrap().ordinal, Some(1));
    }

    #[test]
    fn accessibility_snapshot_keeps_global_ordinal_when_duplicate_is_truncated() {
        let raw: Vec<chromiumoxide::cdp::browser_protocol::accessibility::AxNode> =
            serde_json::from_value(serde_json::json!([{
                "nodeId": "root",
                "ignored": true,
                "childIds": ["1", "2"]
            }, {
                "nodeId": "1",
                "ignored": false,
                "parentId": "root",
                "role": {"type": "role", "value": "textbox"},
                "name": {"type": "computedString", "value": "Phone"}
            }, {
                "nodeId": "2",
                "ignored": false,
                "parentId": "root",
                "role": {"type": "role", "value": "textbox"},
                "name": {"type": "computedString", "value": "Phone"}
            }]))
            .expect("valid CDP AX fixture");

        let (nodes, truncated) = compact_ax_tree(&raw, 1);

        assert!(truncated);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].target.as_ref().unwrap().ordinal, Some(0));
    }

    // These cover only the Chrome-specific layer: `WorkerId`-keyed PID-registry
    // paths and the "chrom" identity check. The reap/register/kill mechanics are
    // tested in `process_registry`.

    #[test]
    fn register_and_unregister_chrome_pid_round_trip_through_the_filesystem() {
        use tempfile::tempdir;
        use types::WorkerId;

        let registry_dir = tempdir().unwrap();
        let worker_id = WorkerId::new();
        let path = super::register_chrome_pid(registry_dir.path(), &worker_id, 4_242)
            .expect("registering a PID under a writable directory must succeed");
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "4242");

        super::unregister_chrome_pid(&path);
        assert!(!path.exists());
    }

    #[test]
    fn register_chrome_pid_is_best_effort_under_an_unwritable_registry_dir() {
        use types::WorkerId;

        let unwritable = PathBuf::from("/this/path/does/not/exist/and/cannot/be/created");
        let worker_id = WorkerId::new();
        assert!(super::register_chrome_pid(&unwritable, &worker_id, 1).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn is_running_chrome_process_matches_on_a_chrom_substring_not_an_exact_name() {
        use std::process::Command;

        // `sleep` never contains "chrom", so a live non-Chrome process must
        // be identified as not Chrome.
        let mut child = Command::new("sleep").arg("5").spawn().unwrap();
        assert!(!super::is_running_chrome_process(child.id()));
        child.kill().unwrap();
        let _ = child.wait();
    }

    #[test]
    fn js_timeout_is_clamped_to_the_configured_ceiling_but_never_raised() {
        assert_eq!(clamp_js_timeout_ms(120_000, 30_000), 30_000);
        assert_eq!(clamp_js_timeout_ms(5_000, 30_000), 5_000);
        assert_eq!(clamp_js_timeout_ms(u64::MAX, 30_000), 30_000);
        assert_eq!(clamp_js_timeout_ms(0, 30_000), 0);
    }

    #[test]
    fn wait_text_matchers_are_bounded_and_deterministic() {
        assert!(text_matches(&TextMatch::Exact("ready".into()), "ready").unwrap());
        assert!(text_matches(&TextMatch::Contains("ead".into()), "ready").unwrap());
        assert!(text_matches(&TextMatch::Regex("^rea.*$".into()), "ready").unwrap());
        assert_eq!(
            text_matches(&TextMatch::Regex("x".repeat(257)), "ready")
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
    }

    #[tokio::test]
    async fn failed_cookie_application_does_not_advance_state() {
        let mut state = HttpBridgeState {
            version: 7,
            cache_validators: BTreeMap::from([("etag".into(), "old".into())]),
        };
        let result = apply_state_commit(
            &mut state,
            8,
            BTreeMap::from([("etag".into(), "new".into())]),
            async { Err::<(), _>("CDP rejected cookies") },
        )
        .await;

        assert_eq!(result.unwrap_err(), "CDP rejected cookies");
        assert_eq!(state.version, 7);
        assert_eq!(state.cache_validators.get("etag").unwrap(), "old");
    }

    #[test]
    fn opaque_partitioned_cookie_fails_closed() {
        let cookie = Cookie {
            name: "partitioned".into(),
            value: "secret".into(),
            domain: "example.test".into(),
            path: "/".into(),
            expires: -1.0,
            size: 17,
            http_only: true,
            secure: true,
            session: true,
            same_site: None,
            priority: CookiePriority::Medium,
            source_scheme: CookieSourceScheme::Secure,
            source_port: 443,
            partition_key: None,
            partition_key_opaque: Some(true),
        };

        let error = match snapshot_cookie(cookie) {
            Ok(_) => panic!("opaque partitioned cookie must fail closed"),
            Err(error) => error,
        };
        assert_eq!(error.code, ErrorCode::HttpEquivalenceUnproven);
    }
}
