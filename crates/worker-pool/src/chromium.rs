use std::collections::{BTreeMap, HashMap, HashSet};
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
use chromiumoxide::browser::{Browser, BrowserConfig as ChromiumConfig, BrowserHandle};
use chromiumoxide::cdp::browser_protocol::browser::{
    DownloadProgressState, EventDownloadProgress, EventDownloadWillBegin,
    SetDownloadBehaviorBehavior, SetDownloadBehaviorParams,
};
use chromiumoxide::cdp::browser_protocol::emulation::{
    MediaFeature, SetEmulatedMediaParams, SetFocusEmulationEnabledParams,
};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchMouseEventParams, DispatchMouseEventType, MouseButton,
};
use chromiumoxide::cdp::browser_protocol::network::{
    Cookie, CookieParam, CookiePartitionKey, CookiePriority, CookieSameSite, CookieSourceScheme,
    SetCookiesParams, TimeSinceEpoch,
};
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, EventJavascriptDialogClosed, EventJavascriptDialogOpening, Viewport,
};
use chromiumoxide::cdp::browser_protocol::target::{EventTargetCreated, TargetId};
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
        gather_candidates, inspect_page_scoped_target, resolve_target as resolve_browser_target,
        resolve_target_with_visibility, TARGET_GONE_MESSAGE,
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
        // The fingerprint screen is spoofed at the JS layer, but CSS media
        // queries are evaluated natively against the real window: a default
        // 800x600 headless window contradicts a 1920x1080 spoofed screen and
        // CreepJS's screen query flags it ("like headless"). Size the real
        // window to the spoofed screen so both channels agree.
        if self.fingerprint.enabled {
            builder = builder.window_size(
                self.fingerprint.screen.width,
                self.fingerprint.screen.height,
            );
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
        // Watch the CDP event stream: its end is the first observable moment
        // of a browser death (OOM kill, self-exit, external reaper). Log it
        // with the session/worker ids so "receiver is gone" stops being the
        // first symptom anyone sees.
        let handler_session = session_id.clone();
        let handler_worker = worker_id.clone();
        let handler_task = tokio::spawn(async move {
            let mut end_detail = "stream exhausted".to_owned();
            while let Some(event) = handler.next().await {
                if let Err(error) = event {
                    end_detail = format!("stream error: {error}");
                    break;
                }
            }
            tracing::info!(
                session_id = %handler_session.0,
                worker_id = %handler_worker.0,
                detail = %end_detail,
                "browser CDP event stream ended"
            );
        });

        let behavioral_config = BehavioralConfig::default().sanitize();
        Ok(Arc::new(ChromiumWorker {
            id: worker_id,
            profile_dir,
            pid_registry_path,
            debug_ws_url: Some(browser.websocket_address().clone()),
            upload_roots: self.config.upload_roots.clone(),
            download_dir,
            session_id: session_id.clone(),
            artifacts: ArtifactStore::new(
                self.config.artifacts_dir.clone(),
                self.config.max_artifact_bytes,
                self.config.max_screenshot_dimension,
            ),
            max_screenshot_dimension: self.config.max_screenshot_dimension,
            max_js_result_bytes: self.config.max_js_result_bytes,
            max_js_timeout_ms: self.config.max_js_timeout_ms,
            browser: Mutex::new(Some(browser)),
            pages: Mutex::new(HashMap::new()),
            closed_targets: Mutex::new(HashSet::new()),
            network_trackers: Mutex::new(HashMap::new()),
            har_recorders: Mutex::new(HashMap::new()),
            har_tasks: Mutex::new(HashMap::new()),
            pending_dialogs: Arc::new(Mutex::new(HashMap::new())),
            dialog_tasks: Mutex::new(HashMap::new()),
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
    /// The browser process's CDP websocket endpoint, captured at launch.
    /// Lets `reconnect_live_process` reattach to the SAME process after a
    /// transport-only failure, preserving all page state.
    debug_ws_url: Option<String>,
    upload_roots: Vec<PathBuf>,
    download_dir: PathBuf,
    session_id: SessionId,
    artifacts: ArtifactStore,
    max_screenshot_dimension: u32,
    max_js_result_bytes: usize,
    max_js_timeout_ms: u64,
    browser: Mutex<Option<Browser>>,
    pages: Mutex<HashMap<PageId, Page>>,
    /// Targets this worker closed, kept until the browser reaps them so
    /// `sync_untracked_pages` does not re-adopt a page that is on its way out.
    closed_targets: Mutex<HashSet<TargetId>>,
    network_trackers: Mutex<HashMap<PageId, Arc<crate::network_quiet::NetworkQuietTracker>>>,
    har_recorders: Mutex<HashMap<PageId, Arc<crate::HarRecorder>>>,
    har_tasks: Mutex<HashMap<PageId, JoinHandle<()>>>,
    /// Dialogs (`alert`/`confirm`/`prompt`) that opened on a page before a
    /// client called `dialog`, keyed by page. `install_dialog_listener`
    /// spawns a background task per registered page that records the latest
    /// dialog here — sequential MCP agents call `click` then `dialog`, and
    /// by the time `dialog` runs the JavaScript dialog is usually already
    /// open, so waiting for a *future* `EventJavascriptDialogOpening` (the
    /// old behavior) hangs forever.
    pending_dialogs: Arc<Mutex<HashMap<PageId, Arc<EventJavascriptDialogOpening>>>>,
    dialog_tasks: Mutex<HashMap<PageId, JoinHandle<()>>>,
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
        let page = self
            .pages
            .lock()
            .await
            .get(page_id)
            .cloned()
            .ok_or_else(page_missing)?;
        if !page.is_closed() {
            return Ok(page);
        }
        // The handle's channel is dead (renderer crash or target hiccup).
        // Re-attach to the live target if it still exists; if it is truly
        // gone, drop the registration so the caller gets a clean notFound
        // instead of a dead handle.
        let target_id = page.target_id().clone();
        let reattached = match self.browser_handle().await {
            Ok(browser) => browser.get_page(target_id).await.ok(),
            Err(_) => None,
        };
        match reattached {
            Some(fresh) => {
                let (_, recorder) = self.unregister_page_state(page_id, true).await;
                self.register_page(page_id.clone(), fresh.clone()).await?;
                if let Some(recorder) = recorder {
                    self.install_har_collector(page_id, fresh.clone(), Some(recorder))
                        .await?;
                }
                Ok(fresh)
            }
            None => {
                self.unregister_page(page_id).await;
                Err(page_missing())
            }
        }
    }

    /// Clone a handle to the live browser and release the mutex immediately.
    /// Every page op that needs the browser goes through this instead of
    /// holding the worker-wide mutex across a CDP await: one page's slow or
    /// hung browser call used to serialize (or, with no timeout, permanently
    /// stall) every other page of the session.
    async fn browser_handle(&self) -> Result<BrowserHandle, CommandError> {
        self.browser
            .lock()
            .await
            .as_ref()
            .map(Browser::handle)
            .ok_or_else(closed_error)
    }

    fn control_target_spec(target: &FormControlTarget) -> TargetSpec {
        // An unnamed iframe's stamped hop carries an empty accessibleName;
        // `TargetSpec { accessible_name: Some("") }` can never match a
        // gathered candidate (whose name is `None`, not `Some("")`), so
        // normalize to `None` here rather than relying on validation to
        // relax the empty-field check for every FormControlTarget path.
        fn segment(segment: &types::SemanticTargetSegment) -> Box<TargetSpec> {
            Box::new(TargetSpec {
                role: Some(segment.role.clone()),
                accessible_name: (!segment.accessible_name.is_empty())
                    .then(|| segment.accessible_name.clone()),
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
        modifiers: &[types::ClickModifier],
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
        self.dispatch_click(page, target, modifiers).await?;
        Ok(())
    }

    async fn dispatch_click(
        &self,
        page: &Page,
        target: Point,
        modifiers: &[types::ClickModifier],
    ) -> Result<(), CommandError> {
        if modifiers.is_empty() {
            page.click(target)
                .await
                .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
            return Ok(());
        }

        let modifier_bits = chromium_modifier_bits(modifiers);
        page.move_mouse(target)
            .await
            .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
        for (event_type, buttons) in [
            (DispatchMouseEventType::MousePressed, 1),
            (DispatchMouseEventType::MouseReleased, 0),
        ] {
            let params = DispatchMouseEventParams::builder()
                .r#type(event_type)
                .x(target.x)
                .y(target.y)
                .modifiers(modifier_bits)
                .button(MouseButton::Left)
                .buttons(buttons)
                .click_count(1)
                .build()
                .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
            page.execute(params)
                .await
                .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
        }
        Ok(())
    }

    /// Best-effort `Page.bringToFront`, run immediately before dispatching a
    /// click or keyboard event so the target page is not backgrounded — on a
    /// background target in headless Chrome, chromiumoxide's own click path
    /// (scroll-into-view + clickable-point + `Input.dispatchMouseEvent`)
    /// never resolves. Failure here never aborts the caller: it is logged
    /// and the dispatch proceeds anyway.
    async fn bring_page_to_front(&self, page: &Page) {
        if let Err(error) = bounded_cdp(page.bring_to_front(), command_failed).await {
            tracing::debug!(?error, "bring-to-front before dispatch failed");
        }
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
    ) -> Result<(Arc<crate::HarRecorder>, bool), CommandError> {
        let page = self.page_handle(page_id).await?;
        self.install_har_collector(page_id, page, None).await
    }

    /// Returns whether this call attached the collector fresh (spawned a new
    /// listener task) as opposed to reusing one already live for the page.
    async fn install_har_collector(
        &self,
        page_id: &PageId,
        page: Page,
        recovered_recorder: Option<Arc<crate::HarRecorder>>,
    ) -> Result<(Arc<crate::HarRecorder>, bool), CommandError> {
        // Coordinate page identity and collector installation under one lock
        // order. Unregistration takes these locks in the same order, so a
        // collector for a stale page cannot be inserted after replacement.
        let pages = self.pages.lock().await;
        let current = pages.get(page_id).ok_or_else(page_missing)?;
        if current.is_closed() || current.session_id() != page.session_id() {
            return Err(page_missing());
        }
        let mut recorders = self.har_recorders.lock().await;
        let mut tasks = self.har_tasks.lock().await;
        if recovered_recorder.is_none() {
            if let (Some(recorder), Some(task)) = (recorders.get(page_id), tasks.get(page_id)) {
                if !task.is_finished() {
                    return Ok((recorder.clone(), false));
                }
            }
        }
        let recorder = recovered_recorder
            .or_else(|| recorders.remove(page_id))
            .unwrap_or_else(|| Arc::new(crate::HarRecorder::default()));
        if let Some(task) = tasks.remove(page_id) {
            task.abort();
        }
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
            let mut pending: HashMap<String, (crate::HarEntry, f64)> = HashMap::new();
            loop {
                tokio::select! {
                    event = will_send.next() => {
                        let Some(event) = event else { break };
                        let id = event.request_id.inner().to_owned();
                        let started_monotonic_ms = *event.timestamp.inner() * 1000.0;
                        if let (Some(response), Some((mut redirected, redirect_started_ms))) =
                            (event.redirect_response.as_ref(), pending.remove(&id))
                        {
                            redirected.elapsed_ms = (redirect_started_ms.is_finite()
                                && started_monotonic_ms.is_finite())
                            .then(|| (started_monotonic_ms - redirect_started_ms).max(0.0));
                            redirected.status = u16::try_from(response.status).ok();
                            redirected.status_text = Some(response.status_text.clone());
                            redirected.redirect_url = Some(event.request.url.clone());
                            redirected.transfer_bytes =
                                Some(response.encoded_data_length.max(0.0) as u64);
                            redirected.mime_type = Some(response.mime_type.clone());
                            task_recorder.record(redirected).await;
                        }
                        pending.insert(
                            id,
                            (
                                crate::HarEntry {
                                    url: event.request.url.clone(),
                                    method: event.request.method.clone(),
                                    status: None,
                                    status_text: None,
                                    redirect_url: None,
                                    started_unix_ms: *event.wall_time.inner() * 1000.0,
                                    elapsed_ms: None,
                                    transfer_bytes: None,
                                    mime_type: None,
                                    error_text: None,
                                },
                                started_monotonic_ms,
                            ),
                        );
                    }
                    event = responses.next() => {
                        let Some(event) = event else { break };
                        let id = event.request_id.inner().to_owned();
                        if let Some((entry, _)) = pending.get_mut(&id) {
                            entry.status = Some(event.response.status as u16);
                            entry.status_text = Some(event.response.status_text.clone());
                            entry.mime_type = Some(event.response.mime_type.clone());
                        }
                    }
                    event = finished.next() => {
                        let Some(event) = event else { break };
                        let id = event.request_id.inner().to_owned();
                        if let Some((mut entry, started_monotonic_ms)) = pending.remove(&id) {
                            let finished_monotonic_ms = *event.timestamp.inner() * 1000.0;
                            entry.elapsed_ms = (started_monotonic_ms.is_finite()
                                && finished_monotonic_ms.is_finite())
                            .then(|| (finished_monotonic_ms - started_monotonic_ms).max(0.0));
                            entry.transfer_bytes = Some(event.encoded_data_length as u64);
                            task_recorder.record(entry).await;
                        }
                    }
                    event = failed.next() => {
                        let Some(event) = event else { break };
                        let id = event.request_id.inner().to_owned();
                        if let Some((mut entry, started_monotonic_ms)) = pending.remove(&id) {
                            let failed_monotonic_ms = *event.timestamp.inner() * 1000.0;
                            entry.elapsed_ms = (started_monotonic_ms.is_finite()
                                && failed_monotonic_ms.is_finite())
                            .then(|| (failed_monotonic_ms - started_monotonic_ms).max(0.0));
                            entry.error_text = Some(event.error_text.clone());
                            task_recorder.record(entry).await;
                        }
                    }
                }
            }
        });
        recorders.insert(page_id.clone(), recorder.clone());
        tasks.insert(page_id.clone(), task);
        Ok((recorder, true))
    }

    async fn resolve_target(
        &self,
        page_id: &PageId,
        page: &Page,
        selector: &str,
        target: Option<&types::TargetSpec>,
    ) -> Result<crate::targeting::ResolvedTarget, CommandError> {
        let browser = self.browser_handle().await?;
        resolve_browser_target(page_id, page, selector, target, Some(&browser)).await
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
        self.install_dialog_listener(&page_id, &page).await;
        self.pages.lock().await.insert(page_id, page);
        Ok(())
    }

    /// Spawn a background task that records this page's JavaScript dialogs
    /// as they open, so a dialog already open by the time `dialog` is
    /// called is not missed (see `pending_dialogs`). Best-effort: if the
    /// page cannot hand out a dialog listener, no task is spawned and
    /// `handle_dialog` falls back to its own future-event wait.
    async fn install_dialog_listener(&self, page_id: &PageId, page: &Page) {
        let Ok(mut opened) = page.event_listener::<EventJavascriptDialogOpening>().await else {
            return;
        };
        // A dialog closed by anything other than `handle_dialog` (a
        // navigation, the page itself) must drop its pending record, or the
        // next click on this page would report it as freshly opened.
        let Ok(mut closed) = page.event_listener::<EventJavascriptDialogClosed>().await else {
            return;
        };
        let pending_dialogs = self.pending_dialogs.clone();
        let task_page_id = page_id.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    event = opened.next() => {
                        let Some(event) = event else { break };
                        pending_dialogs
                            .lock()
                            .await
                            .insert(task_page_id.clone(), event);
                    }
                    event = closed.next() => {
                        if event.is_none() {
                            break;
                        }
                        pending_dialogs.lock().await.remove(&task_page_id);
                    }
                }
            }
        });
        if let Some(previous) = self.dialog_tasks.lock().await.insert(page_id.clone(), task) {
            previous.abort();
        }
    }

    /// Poll `pending_dialogs` for a dialog recorded on this page. Only used
    /// to race against an in-flight click (see `click`); never resolves on
    /// its own if no dialog opens, so callers must bound it with a select
    /// against something else that can complete.
    async fn wait_for_pending_dialog(&self, page_id: &PageId) -> (String, String) {
        loop {
            if let Some(event) = self.pending_dialogs.lock().await.get(page_id) {
                return (
                    format!("{:?}", event.r#type).to_lowercase(),
                    event.message.clone(),
                );
            }
            tokio::time::sleep(DIALOG_POLL_INTERVAL).await;
        }
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
            .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error.to_string()))?;

        // The init script spoofs screen metrics at the JS layer, but the
        // browser's virtual screen (screen.width, device-width media
        // queries) is evaluated natively and stays at the headless default
        // 800x600. Override device metrics to the spoofed profile so every
        // channel agrees; without this CreepJS's screen query flags the
        // mismatch ("like headless"). The viewport is the spoofed window
        // size, smaller than the virtual screen — a pixel-exact full-screen
        // viewport is itself a tell (hasVvpScreenRes).
        let (screen_w, screen_h, pixel_ratio, window_w, window_h) = {
            let config = self.fingerprint.lock().await;
            let resolution =
                fingerprinting::ScreenMasker::new(config.screen.clone()).get_spoofed_resolution();
            (
                resolution.width,
                resolution.height,
                resolution.pixel_ratio,
                resolution.window_width.unwrap_or(resolution.width),
                resolution.window_height.unwrap_or(resolution.height),
            )
        };
        if screen_w > 0 && screen_h > 0 {
            let params = chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::builder()
                .width(window_w as i64)
                .height(window_h as i64)
                .device_scale_factor(pixel_ratio)
                .mobile(false)
                .screen_width(screen_w as i64)
                .screen_height(screen_h as i64)
                .build()
                .map_err(|error| driver_error(ErrorCode::InvalidRequest, error))?;
            bounded_cdp(page.execute(params), command_failed).await?;
        }
        Ok(())
    }

    async fn unregister_page_state(
        &self,
        page_id: &PageId,
        preserve_har: bool,
    ) -> (Option<Page>, Option<Arc<crate::HarRecorder>>) {
        self.network_trackers.lock().await.remove(page_id);
        self.pending_dialogs.lock().await.remove(page_id);
        if let Some(task) = self.dialog_tasks.lock().await.remove(page_id) {
            task.abort();
        }
        let mut pages = self.pages.lock().await;
        let mut recorders = self.har_recorders.lock().await;
        let mut tasks = self.har_tasks.lock().await;
        let page = pages.remove(page_id);
        let recorder = recorders.remove(page_id);
        if let Some(task) = tasks.remove(page_id) {
            task.abort();
        }
        (page, preserve_har.then_some(recorder).flatten())
    }

    async fn unregister_page(&self, page_id: &PageId) -> Option<Page> {
        self.unregister_page_state(page_id, false).await.0
    }

    /// Register page targets the runtime did not open — popups from
    /// `window.open` and any other tab the site spawned. One browser serves
    /// one session, so an untracked page target belongs to this session.
    /// Lazy (on `list_pages`) rather than a background listener: the only
    /// consumer is the listing itself.
    async fn sync_untracked_pages(&self) -> Result<(), CommandError> {
        let Ok(browser) = self.browser_handle().await else {
            return Ok(());
        };
        let live = browser.pages().await.map_err(command_failed)?;
        // `Page.close` is acknowledged before the browser finishes destroying
        // the target, so a page this worker just closed can still sit in the
        // handler's target cache. Tombstones keep it from being resurrected
        // here as an "untracked" page, and are dropped once the target is
        // actually gone.
        let closed = {
            let mut closed = self.closed_targets.lock().await;
            closed.retain(|target| live.iter().any(|page| page.target_id() == target));
            closed.clone()
        };
        for page in live {
            if closed.contains(page.target_id()) {
                continue;
            }
            // Browser chrome (new-tab page et al.) is a target but not a
            // session page.
            if page
                .url()
                .await
                .ok()
                .flatten()
                .is_some_and(|url| url.starts_with("chrome://"))
            {
                continue;
            }
            let known = self
                .pages
                .lock()
                .await
                .values()
                .any(|known| known.target_id() == page.target_id());
            if !known {
                // Registration talks to the target over CDP, so a target that
                // died between the listing and the attach (the site's own
                // `window.close`, or a close whose destruction the browser has
                // not finished) fails with a dead-session error. That target is
                // not a page of this session: skip it instead of failing the
                // whole listing.
                if let Err(error) = self.register_page(PageId::new(), page).await {
                    if is_closed_page_message(&error.message) {
                        continue;
                    }
                    return Err(error);
                }
            }
        }
        Ok(())
    }
    /// Reattach to the same browser process after the CDP websocket died
    /// without the process dying: check the child PID, connect a fresh
    /// browser-level websocket, and re-register every still-live target
    /// under its existing page id. State on the pages (typed values, DOM)
    /// is untouched — only the transport is new.
    ///
    /// Returns Ok(evidence) when reattached; Err with the original shaped
    /// error when the process is really gone (the caller then proceeds with
    /// the relaunch path).
    async fn reconnect_live_process_impl(&self) -> Result<Vec<Evidence>, CommandError> {
        let Some(mut browser) = self.browser.lock().await.take() else {
            return Err(closed_error());
        };
        // Abort the old handler: its websocket is dead either way.
        if let Some(task) = self.handler_task.lock().await.take() {
            task.abort();
        }
        // Process-alive verdict: prefer the child handle's exit status (no
        // subprocess spawn); the PID registry is the fallback when the child
        // was never readable.
        let child_alive = match browser.get_mut_child() {
            Some(child) => child.try_wait().ok().flatten().is_none(),
            None => self
                .pid_registry_path
                .as_deref()
                .and_then(|path| std::fs::read_to_string(path).ok())
                .and_then(|text| text.trim().parse::<u32>().ok())
                .and_then(process_registry::process_command_name)
                .is_some_and(|name| name.contains("chrom")),
        };
        if !child_alive {
            // Really dead: hand the browser back so the caller's relaunch
            // path can register/unregister against it as before.
            *self.browser.lock().await = Some(browser);
            return Err(closed_error());
        }
        let Some(url) = &self.debug_ws_url else {
            *self.browser.lock().await = Some(browser);
            return Err(closed_error());
        };
        let (fresh, mut handler) = match Browser::connect(url.clone()).await {
            Ok(pair) => pair,
            Err(error) => {
                tracing::warn!(
                    session_id = %self.session_id.0,
                    worker_id = %self.id.0,
                    "CDP reattach to live browser failed: {error}"
                );
                *self.browser.lock().await = Some(browser);
                return Err(closed_error());
            }
        };
        let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });
        *self.handler_task.lock().await = Some(handler_task);
        // Swap the fresh connection in, then reconcile the page registry.
        // The old page handles reference the dead browser's channels: each
        // tracked page is re-resolved against the live browser by target id.
        // A target that died WITH the transport (renderer crash alongside the
        // reset) is dropped, so page_handle reports a clean page_missing.
        let tracked: Vec<(
            PageId,
            chromiumoxide::cdp::browser_protocol::target::TargetId,
        )> = {
            let pages = self.pages.lock().await;
            pages
                .iter()
                .map(|(page_id, page)| (page_id.clone(), page.target_id().clone()))
                .collect()
        };
        *self.browser.lock().await = Some(fresh);
        {
            let mut pages = self.pages.lock().await;
            pages.clear();
        }
        {
            let handle = self.browser_handle().await?;
            for (page_id, target_id) in tracked {
                match handle.get_page(target_id).await {
                    Ok(live) => {
                        self.register_page(page_id, live).await?;
                    }
                    Err(_) => {
                        // Target lost with the transport: drop the page so
                        // callers get notFound instead of a stale handle.
                        self.unregister_page_state(&page_id, true).await;
                    }
                }
            }
        }
        tracing::info!(
            session_id = %self.session_id.0,
            worker_id = %self.id.0,
            "reattached to live browser after CDP transport reset; page state preserved"
        );
        Ok(vec![Evidence::Configuration {
            name: "cdpReattach".into(),
            value: "websocket reset with the browser process still alive; reattached \
                    to the same process and page state is preserved"
                .into(),
        }])
    }
}

const PLAIN_CLICK_TARGET_DRIFT_RETRIES: usize = 3;
const PLAIN_CLICK_TARGET_DRIFT_DELAY: Duration = Duration::from_millis(25);
/// How often `click` polls for a dialog having opened while its own click
/// future is still pending.
const DIALOG_POLL_INTERVAL: Duration = Duration::from_millis(50);

fn should_retry_plain_click_target_drift(
    boundary: bool,
    attempt: usize,
    error: &CommandError,
) -> bool {
    !boundary
        && attempt < PLAIN_CLICK_TARGET_DRIFT_RETRIES
        && error.code == ErrorCode::TargetNotFound
}

fn ensure_automatic_download_modifier_support(
    modifiers: &[types::ClickModifier],
) -> Result<(), CommandError> {
    if modifiers.is_empty() {
        return Ok(());
    }
    Err(CommandError {
        code: ErrorCode::InvalidRequest,
        message: "click modifiers are not supported by automatic download capture".into(),
        layer: ErrorLayer::Driver,
        retryable: false,
    })
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
        let browser = self.browser_handle().await?;
        let page = browser
            .new_page("about:blank")
            .await
            .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
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
        .map_err(|error| navigation_failed(command.url.as_str(), error))?;
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
        let (text, html, resolution) = if command
            .target
            .as_ref()
            .is_some_and(is_page_scoped_text_target)
        {
            let target = command.target.as_ref().expect("checked target");
            let browser = self.browser_handle().await?;
            let (text, html, resolution) = inspect_page_scoped_target(
                page_id,
                &page,
                target,
                command.include_html,
                Some(&browser),
            )
            .await?;
            (text, html, Some(resolution))
        } else if command.selector.is_some() || command.target.is_some() {
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
        for attempt in 0..=PLAIN_CLICK_TARGET_DRIFT_RETRIES {
            let resolved = match self
                .resolve_target(page_id, &page, &command.selector, command.target.as_ref())
                .await
            {
                Ok(resolved) => resolved,
                Err(error)
                    if should_retry_plain_click_target_drift(command.boundary, attempt, &error) =>
                {
                    tokio::time::sleep(PLAIN_CLICK_TARGET_DRIFT_DELAY).await;
                    continue;
                }
                Err(error) => return Err(error),
            };
            // A plain click on a download link otherwise vanishes: headless
            // Chromium drops the file and the outcome carries no evidence.
            // Route through the armed capture path so the file lands in the
            // session's downloads with Download evidence.
            match resolved.is_download_link(&page).await {
                Ok(true) => {
                    ensure_automatic_download_modifier_support(&command.modifiers)?;
                    return self
                        .click_and_wait_for_download(
                            page_id,
                            &ClickAndWaitForDownloadCommand {
                                selector: command.selector.clone(),
                                target: command.target.clone(),
                                timeout_ms: 30_000,
                            },
                        )
                        .await;
                }
                Ok(false) => {}
                Err(error)
                    if should_retry_plain_click_target_drift(command.boundary, attempt, &error) =>
                {
                    tokio::time::sleep(PLAIN_CLICK_TARGET_DRIFT_DELAY).await;
                    continue;
                }
                Err(error) => return Err(error),
            }
            let text = resolved.inner_text(&page).await.ok().flatten();
            self.bring_page_to_front(&page).await;
            let mut click_future: std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<(), CommandError>> + Send + '_>,
            > = if self.humanization_enabled() {
                Box::pin(self.humanized_click(&page, &resolved, &command.modifiers))
            } else if command.modifiers.is_empty() {
                Box::pin(resolved.click(&page))
            } else {
                let target = resolved.clickable_point(&page).await?.ok_or_else(|| {
                    driver_error(
                        ErrorCode::BrowserCommandFailed,
                        "target has no clickable point",
                    )
                })?;
                Box::pin(self.dispatch_click(&page, target, &command.modifiers))
            };
            // A click that opens alert()/confirm()/prompt() blocks the
            // renderer, so the click's own CDP round trip may never return
            // a response. Race it against a dialog having opened on this
            // page instead of waiting on it unconditionally.
            let dialog = tokio::select! {
                result = &mut click_future => {
                    drop(click_future);
                    match result {
                        Ok(()) => {
                            return Ok(vec![
                                Evidence::Element {
                                    selector: command.selector.clone(),
                                    text,
                                },
                                resolved.evidence,
                            ]);
                        }
                        Err(error)
                            if should_retry_plain_click_target_drift(
                                command.boundary,
                                attempt,
                                &error,
                            ) =>
                        {
                            tokio::time::sleep(PLAIN_CLICK_TARGET_DRIFT_DELAY).await;
                            continue;
                        }
                        Err(error) => return Err(error),
                    }
                }
                dialog = self.wait_for_pending_dialog(page_id) => dialog,
            };
            // A dialog opened while the click future was still resolving.
            // The renderer is blocked showing it, so that future's own CDP
            // round trip may never get a response — don't wait on it.
            // The dialog firing is proof the click landed; report it right
            // away so the caller can call `dialog` next.
            drop(click_future);
            return Ok(vec![
                Evidence::Element {
                    selector: command.selector.clone(),
                    text,
                },
                resolved.evidence,
                Evidence::Configuration {
                    name: "dialogOpened".into(),
                    value: format!("{}: {}", dialog.0, dialog.1),
                },
            ]);
        }
        unreachable!("plain-click target drift retries are bounded")
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
        ensure_editable_control(&page, &resolved).await?;
        self.bring_page_to_front(&page).await;
        let mut humanization_evidence = None;
        let (observed, control_kind) = if resolved.is_checkable(&page).await? {
            let checked = command.value.parse::<bool>().map_err(|_| {
                driver_error(
                    ErrorCode::InvalidRequest,
                    "checkable controls require a boolean value",
                )
            })?;
            (
                resolved.set_checked(&page, checked).await?.to_string(),
                "checkable",
            )
        } else if resolved.is_select(&page).await? {
            (
                resolved.select_option(&page, &command.value).await?,
                "select",
            )
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
            (resolved.value(&page).await?.unwrap_or_default(), "text")
        };
        let validity = resolved.form_control_validity(&page).await?;
        let mut evidence = vec![
            Evidence::Element {
                selector: command.selector.clone(),
                text: Some(observed),
            },
            Evidence::Configuration {
                name: "typedControlKind".into(),
                value: control_kind.into(),
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
                .find(|control| {
                    control.target.as_ref().is_some_and(|target| {
                        crate::target_specs_equivalent(target, &command.target)
                    })
                })
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
        self.bring_page_to_front(&page).await;
        let mut committed: Option<Vec<String>> = None;
        match &command.action {
            ControlAction::SetText { value, clear_first } => {
                resolved.type_text(&page, value, *clear_first).await?
            }
            ControlAction::SetChecked { checked } => {
                resolved.set_checked(&page, *checked).await?;
            }
            ControlAction::SelectOne { value } => {
                committed = Some(vec![resolved.select_option(&page, value).await?]);
            }
            ControlAction::SelectMany { values } => {
                committed = Some(resolved.select_options(&page, values).await?);
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
        let mut evidence = crate::control_action_evidence(
            &after_control,
            &command.action,
            false,
            committed.as_deref(),
        )?;
        // Conditional fields: controls that exist only after this action
        // (a plan select revealing a billing-cycle select). Surfacing them
        // here saves the agent a blind re-snapshot to discover them.
        let before_targets: Vec<&types::FormControlTarget> = before
            .forms
            .iter()
            .flat_map(|form| form.controls.iter())
            .chain(before.unowned_controls.iter())
            .filter_map(|control| control.target.as_ref())
            .collect();
        evidence.revealed_controls = after
            .forms
            .iter()
            .flat_map(|form| form.controls.iter())
            .chain(after.unowned_controls.iter())
            .filter(|control| {
                control.target.as_ref().is_some_and(|target| {
                    !before_targets
                        .iter()
                        .any(|before| crate::target_specs_equivalent(before, target))
                })
            })
            .take(8)
            .map(|control| types::RevealedControl {
                control_kind: control.control_kind,
                accessible_name: control.accessible_name.clone(),
                target: control.target.clone(),
            })
            .collect();
        Ok(vec![Evidence::ControlAction { action: evidence }])
    }

    async fn open_page_command(
        &self,
        command: &OpenPageCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let page_id = PageId::new();
        let browser = self.browser_handle().await?;
        // Always start blank so init scripts register before first real document.
        let page = browser
            .new_page("about:blank")
            .await
            .map_err(command_failed)?;
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
        self.sync_untracked_pages().await?;
        let handles: Vec<(PageId, Page)> = self
            .pages
            .lock()
            .await
            .iter()
            .map(|(page_id, page)| (page_id.clone(), page.clone()))
            .collect();
        let mut listed = Vec::with_capacity(handles.len());
        for (page_id, page) in handles {
            match page_evidence(page_id.clone(), &page).await {
                Ok(evidence) => listed.push(evidence),
                Err(error) if is_closed_page_message(&error.message) => {
                    self.unregister_page(&page_id).await;
                }
                Err(error) => return Err(error),
            }
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
        self.closed_targets
            .lock()
            .await
            .insert(page.target_id().clone());
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
        let page = self.page_handle(page_id).await?;
        let (recorder, attached_fresh) = self.ensure_har_collector(page_id).await?;
        let entries = recorder.take(command.clear).await;
        let page_url = page.url().await.ok().flatten().unwrap_or_default();
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
        let mut evidence = vec![Evidence::HarArtifact {
            artifact_id: record.artifact_id,
            media_type: record.media_type,
            bytes: record.bytes,
            sha256: record.sha256,
            entries: entries.len() as u32,
        }];
        if attached_fresh {
            evidence.push(Evidence::Configuration {
                name: "networkRecordingStarted".into(),
                value: "true".into(),
            });
        }
        Ok(evidence)
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
        let accept = matches!(command.action, types::DialogAction::Accept);
        // The dialog may already be open — a click that triggered it always
        // runs before the client asks about it — so check the recorded
        // pending dialog first and only fall back to waiting on a future
        // event if none was captured.
        let event = if let Some(event) = self.pending_dialogs.lock().await.remove(page_id) {
            event
        } else {
            let mut events = page
                .event_listener::<EventJavascriptDialogOpening>()
                .await
                .map_err(command_failed)?;
            tokio::time::timeout(timeout, events.next())
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
                .ok_or_else(|| driver_error(ErrorCode::NotFound, "dialog event stream closed"))?
        };
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
        let max_nodes = command
            .max_nodes
            .unwrap_or(DEFAULT_A11Y_MAX_NODES)
            .clamp(1, MAX_A11Y_NODES) as usize;
        if let Some(target) = &command.target {
            // Scoped: resolve the target, fetch the AX tree of the frame it
            // lives in, and compact only its subtree.
            let resolved = {
                let browser = self.browser_handle().await?;
                crate::targeting::resolve_target(page_id, &page, "", Some(target), Some(&browser))
                    .await?
            };
            let backend_id = resolved.backend_node_id(&page).await?;
            // Scoping to the iframe element itself must return the frame's
            // content — the main-frame AX subtree of an iframe node is empty,
            // and the agent asked for what is inside.
            let owner_frame = page
                .execute(
                    chromiumoxide::cdp::browser_protocol::dom::DescribeNodeParams::builder()
                        .backend_node_id(backend_id)
                        .build(),
                )
                .await
                .map_err(command_failed)?
                .result
                .node
                .frame_id;
            if let Some(content_frame) = owner_frame {
                let result = page
                    .execute(
                        chromiumoxide::cdp::browser_protocol::accessibility::GetFullAxTreeParams::builder()
                            .frame_id(content_frame)
                            .build(),
                    )
                    .await
                    .map_err(command_failed)?
                    .result;
                let (mut nodes, truncated) = compact_ax_tree(&result.nodes, max_nodes);
                // Stamp targets with the hop that re-resolves the iframe
                // element, so in-frame targets pass to control_action
                // verbatim like the full-tree descent's do.
                let hop_name = match &resolved.evidence {
                    Evidence::Resolution { fingerprint, .. } => fingerprint.name.clone(),
                    _ => None,
                };
                if let Some(name) = hop_name {
                    let segment = types::SemanticTargetSegment {
                        role: "iframe".into(),
                        accessible_name: name,
                        ordinal: None,
                    };
                    stamp_a11y_frame_path(&mut nodes, &segment);
                }
                return Ok(vec![Evidence::AccessibilitySnapshot {
                    page_id: page_id.clone(),
                    nodes,
                    truncated,
                }]);
            }
            let params = match resolved.frame_id() {
                Some(frame_id) => {
                    chromiumoxide::cdp::browser_protocol::accessibility::GetFullAxTreeParams::builder()
                        .frame_id(frame_id.clone())
                        .build()
                }
                None => {
                    chromiumoxide::cdp::browser_protocol::accessibility::GetFullAxTreeParams::default()
                }
            };
            let result = page.execute(params).await.map_err(command_failed)?.result;
            let root_id = result
                .nodes
                .iter()
                .find(|node| node.backend_dom_node_id.as_ref() == Some(&backend_id))
                .map(|node| node.node_id.clone())
                .ok_or_else(|| {
                    driver_error(
                        ErrorCode::TargetNotFound,
                        "resolved target has no accessibility node",
                    )
                })?;
            let (nodes, truncated) =
                compact_ax_tree_from(&result.nodes, max_nodes, Some(root_id.as_ref()));
            return Ok(vec![Evidence::AccessibilitySnapshot {
                page_id: page_id.clone(),
                nodes,
                truncated,
            }]);
        }
        let result = page
            .execute(
                chromiumoxide::cdp::browser_protocol::accessibility::GetFullAxTreeParams::default(),
            )
            .await
            .map_err(command_failed)?
            .result;
        let (mut nodes, mut truncated) = compact_ax_tree(&result.nodes, max_nodes);
        // The main-frame AX tree stops at `Iframe` nodes; descend one level
        // (cap 8, same-process frames) so in-frame controls are visible and
        // their targets carry the frame hop control_action can re-resolve.
        let emitted = count_a11y_nodes(&nodes);
        if emitted < max_nodes {
            truncated |= descend_a11y_iframes(&page, &mut nodes, max_nodes - emitted).await;
        }
        let controls_omitted = if a11y_contains_form_control(&nodes) {
            false
        } else {
            match page
                .evaluate(crate::form_snapshot_expression(page_id))
                .await
            {
                Ok(remote) => {
                    let value: Result<serde_json::Value, _> = remote.into_value();
                    value
                        .ok()
                        .and_then(|value| value.as_str().map(str::to_owned))
                        .and_then(|encoded| {
                            crate::decode_form_snapshot(page_id.clone(), &encoded, 512).ok()
                        })
                        .is_some_and(|snapshot| {
                            !snapshot.unowned_controls.is_empty()
                                || snapshot.forms.iter().any(|form| !form.controls.is_empty())
                        })
                }
                Err(_) => false,
            }
        };
        let mut evidence = vec![Evidence::AccessibilitySnapshot {
            page_id: page_id.clone(),
            nodes,
            truncated,
        }];
        if controls_omitted {
            evidence.push(Evidence::Configuration {
                name: "accessibilityControlsOmitted".into(),
                value: "true: form controls exist but were absent from the accessibility tree; use form_snapshot".into(),
            });
        }
        Ok(evidence)
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
        let browser = self.browser_handle().await?;
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
            Some(&browser),
        )
        .await?;
        // Same H3 class as `click`: a native element click on a backgrounded
        // tab hangs to the CDP request timeout, so surface the opener first.
        self.bring_page_to_front(&opener).await;
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
                sha256: hex::encode(Sha256::digest(&bytes)),
                saved_to: path
                    .strip_prefix(&self.download_dir)
                    .ok()
                    .map(|relative| relative.to_string_lossy().into_owned()),
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
            let poll = match wait_condition_satisfied(
                &self.browser,
                page_id,
                &page,
                tracker.as_deref(),
                &command.condition,
                &mut quiet_since,
            )
            .await
            {
                Ok(poll) => poll,
                // A frame or document navigation can replace its execution
                // context between target resolution and observation. Waits
                // are read-only and already bounded, so reacquire the frame
                // on the next poll instead of converting a successful submit
                // into an immediate false failure.
                Err(error) if wait_should_retry_replaced_context(&error) => {
                    WaitPoll::matched(false)
                }
                Err(error) => return Err(error),
            };
            if poll.satisfied {
                return Ok(vec![Evidence::Wait {
                    condition: command.condition.clone(),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    observations,
                    excluded_classes: poll.excluded_classes,
                    observed: poll.observed.map(|value| bound_observed(&value)),
                }]);
            }
            if Instant::now() >= deadline {
                // Page-scoped text waits often race an async UI confirmation
                // (fetch-then-append). One last body read before timing out.
                if let types::WaitCondition::Text { target, matcher } = &command.condition {
                    if is_page_scoped_text_target(target) {
                        if let Ok(value) =
                            read_page_scoped_text(&self.browser, page_id, &page, target).await
                        {
                            if text_matches(matcher, &value).unwrap_or(false) {
                                return Ok(vec![Evidence::Wait {
                                    condition: command.condition.clone(),
                                    elapsed_ms: started.elapsed().as_millis() as u64,
                                    observations,
                                    excluded_classes: Vec::new(),
                                    observed: Some(bound_observed(&value)),
                                }]);
                            }
                        }
                    }
                }
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
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn collect_candidates(
        &self,
        page_id: &PageId,
        target: &types::TargetSpec,
    ) -> Result<Vec<dom_engine::Candidate>, CommandError> {
        let page = self.page_handle(page_id).await?;
        let browser = self.browser_handle().await?;
        gather_candidates(&page, target, Some(&browser)).await
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
                validate_clip(*x, *y, *width, *height, self.max_screenshot_dimension)?;
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

    async fn element_at_point(
        &self,
        page_id: &PageId,
        x: f64,
        y: f64,
    ) -> Result<Option<(String, String)>, CommandError> {
        let page = self.page_handle(page_id).await?;
        let expression = format!(
            r#"(() => {{
const INTERACTIVE = "a,button,input,select,textarea,[role],[tabindex]";
const IMPLICIT = {{a:"link",button:"button",input:"textbox",select:"combobox",textarea:"textbox"}};
let el = document.elementFromPoint({x}, {y});
if (!el) return null;
const host = el.closest(INTERACTIVE) || el;
let role = host.getAttribute("role") || IMPLICIT[host.tagName.toLowerCase()] || "";
if (host.tagName === "INPUT" && !host.getAttribute("role")) {{
  const t = (host.getAttribute("type") || "text").toLowerCase();
  if (["button","submit","reset"].includes(t)) role = "button";
  else if (t === "checkbox") role = "checkbox";
  else if (t === "radio") role = "radio";
  else if (t === "range") role = "slider";
  else if (t === "search") role = "searchbox";
}}
let name = host.getAttribute("aria-label") || "";
if (!name && host.hasAttribute("aria-labelledby")) {{
  name = host.getAttribute("aria-labelledby").split(/\s+/).map(id => {{
    const n = document.getElementById(id);
    return n ? n.textContent.trim() : "";
  }}).filter(Boolean).join(" ");
}}
if (!name && host.id) {{
  const label = document.querySelector(`label[for="${{host.id}}"]`);
  if (label) name = label.textContent.trim();
}}
if (!name) name = host.getAttribute("placeholder") || "";
if (!name) name = host.getAttribute("title") || "";
if (!name) name = (host.innerText || host.value || "").trim();
if (!role && !name) return null;
return [role, name.slice(0, 200)];
}})()"#
        );
        let mut params = EvaluateParams::new(expression);
        params.return_by_value = Some(true);
        let value: serde_json::Value =
            tokio::time::timeout(Duration::from_millis(2_000), page.evaluate(params))
                .await
                .map_err(|_| timeout_error(2_000))?
                .map_err(command_failed)?
                .into_value()
                .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
        if value.is_null() {
            return Ok(None);
        }
        let pair: (String, String) = serde_json::from_value(value)
            .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
        Ok(Some(pair))
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
            // Teardown must not fail because the browser is already dead:
            // an uncloseable-but-gone browser would wedge the session in the
            // registry forever (every session_close retry failing the same
            // way, pages stuck listed).
            if let Err(error) = browser.close().await {
                let message = error.to_string();
                if !is_closed_page_message(&message) {
                    return Err(command_failed(error));
                }
                tracing::info!("browser already gone during close: {message}");
            }
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
            match browser.close().await {
                Ok(_) => Ok(()),
                Err(error) if is_closed_page_message(&error.to_string()) => {
                    tracing::info!("browser already gone during termination: {error}");
                    Ok(())
                }
                Err(error) => Err(command_failed(error)),
            }
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

    async fn reconnect_live_process(&self) -> Result<Vec<Evidence>, CommandError> {
        self.reconnect_live_process_impl().await
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

/// Truncates a wait observation on a character boundary.
///
/// Byte-index truncation panics inside a multi-byte codepoint, which is how
/// extraction used to die on any non-ASCII page.
fn bound_observed(value: &str) -> String {
    match value.char_indices().nth(types::MAX_WAIT_OBSERVED_CHARS) {
        Some((index, _)) => value[..index].to_owned(),
        None => value.to_owned(),
    }
}

/// What one wait poll saw.
///
/// `observed` is the value the condition's matcher ran against, so a satisfied
/// wait can report it instead of throwing it away. Only the conditions that
/// read a value carry one: `Text`, `Value`, `Url`, and `Document`. `Element`
/// and `NetworkQuiet` match on presence and counts, not on a value, so theirs
/// stays `None` rather than inventing a string.
struct WaitPoll {
    satisfied: bool,
    excluded_classes: Vec<String>,
    observed: Option<String>,
}

impl WaitPoll {
    fn matched(satisfied: bool) -> Self {
        Self {
            satisfied,
            excluded_classes: Vec::new(),
            observed: None,
        }
    }

    fn saw(satisfied: bool, observed: impl Into<String>) -> Self {
        Self {
            satisfied,
            excluded_classes: Vec::new(),
            observed: Some(observed.into()),
        }
    }
}

async fn wait_condition_satisfied(
    browser: &Mutex<Option<Browser>>,
    page_id: &PageId,
    page: &Page,
    tracker: Option<&crate::network_quiet::NetworkQuietTracker>,
    condition: &WaitCondition,
    quiet_since: &mut Option<Instant>,
) -> Result<WaitPoll, CommandError> {
    match condition {
        WaitCondition::Element { target, state } => {
            let resolved = if let Some(selector) = unscoped_css_wait_selector(target) {
                resolve_target_with_visibility(page_id, page, selector, None, false, None).await
            } else {
                let handle = browser.lock().await.as_ref().map(Browser::handle);
                match handle {
                    Some(handle) => {
                        resolve_target_with_visibility(
                            page_id,
                            page,
                            "",
                            Some(target),
                            false,
                            Some(&handle),
                        )
                        .await
                    }
                    None => Err(closed_error()),
                }
            };
            let resolved = match resolved {
                Ok(resolved) => resolved,
                Err(error) => {
                    if let Some(satisfied) = element_wait_missing_observation(state, &error) {
                        return Ok(WaitPoll::matched(satisfied));
                    }
                    return Err(error);
                }
            };
            let observation = match state {
                types::ElementState::Attached => true,
                types::ElementState::Detached => match resolved.visible(page).await {
                    Ok(_) => false,
                    Err(error) => match element_wait_missing_observation(state, &error) {
                        Some(satisfied) => satisfied,
                        None => return Err(error),
                    },
                },
                types::ElementState::Visible | types::ElementState::Hidden => {
                    match resolved.visible(page).await {
                        Ok(visible) => matches!(state, types::ElementState::Visible) == visible,
                        Err(error) => match element_wait_missing_observation(state, &error) {
                            Some(satisfied) => satisfied,
                            None => return Err(error),
                        },
                    }
                }
                types::ElementState::Enabled | types::ElementState::Disabled => {
                    match resolved.enabled(page).await {
                        Ok(enabled) => matches!(state, types::ElementState::Enabled) == enabled,
                        Err(error) => match element_wait_missing_observation(state, &error) {
                            Some(satisfied) => satisfied,
                            None => return Err(error),
                        },
                    }
                }
            };
            Ok(WaitPoll::matched(observation))
        }
        WaitCondition::Text { target, matcher } | WaitCondition::Value { target, matcher } => {
            let is_value = matches!(condition, WaitCondition::Value { .. });
            // a11y / landmark page-scoped roles (and css:body) are not a single
            // node; read live document.body.innerText via evaluate so polls see
            // async UI updates the same way a whole-page inspect does.
            if !is_value && is_page_scoped_text_target(target) {
                let value = read_page_scoped_text(browser, page_id, page, target).await?;
                return Ok(WaitPoll::saw(text_matches(matcher, &value)?, value));
            }
            let handle = browser.lock().await.as_ref().map(Browser::handle);
            let resolved = match handle {
                Some(handle) => {
                    resolve_browser_target(page_id, page, "", Some(target), Some(&handle)).await
                }
                None => Err(closed_error()),
            };
            let resolved = match resolved {
                Ok(resolved) => resolved,
                Err(error) if matches!(error.code, ErrorCode::TargetNotFound) => {
                    return Ok(WaitPoll::matched(false))
                }
                Err(error) => return Err(error),
            };
            let value = if is_value {
                resolved.value(page).await?.unwrap_or_default()
            } else {
                resolved.inner_text(page).await?.unwrap_or_default()
            };
            Ok(WaitPoll::saw(text_matches(matcher, &value)?, value))
        }
        WaitCondition::Url { matcher } => {
            let url = page
                .url()
                .await
                .map_err(command_failed)?
                .unwrap_or_default();
            Ok(WaitPoll::saw(text_matches(matcher, &url)?, url))
        }
        WaitCondition::Document { ready } => {
            let state: String = page
                .evaluate("document.readyState")
                .await
                .map_err(command_failed)?
                .into_value()
                .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
            Ok(WaitPoll::saw(
                match ready {
                    WaitUntil::Commit => true,
                    WaitUntil::DomContentLoaded | WaitUntil::Interactive => {
                        state == "interactive" || state == "complete"
                    }
                    WaitUntil::NetworkIdle => state == "complete",
                },
                state,
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
                Ok(WaitPoll {
                    satisfied: since.elapsed() >= Duration::from_millis(*idle_ms),
                    excluded_classes,
                    observed: None,
                })
            } else {
                *quiet_since = None;
                Ok(WaitPoll {
                    satisfied: false,
                    excluded_classes,
                    observed: None,
                })
            }
        }
    }
}

fn unscoped_css_wait_selector(target: &TargetSpec) -> Option<&str> {
    target.css.as_deref().filter(|_| {
        target.test_id.is_none()
            && target.role.is_none()
            && target.accessible_name.is_none()
            && target.label.is_none()
            && target.text.is_none()
            && target.attributes.is_empty()
            && target.frame_path.is_empty()
            && target.shadow_path.is_empty()
            && target.ordinal.is_none()
    })
}

fn is_missing_css_node(error: &CommandError) -> bool {
    error.code == ErrorCode::BrowserCommandFailed
        && error.message.contains("Could not find node with given id")
}

fn element_wait_missing_observation(
    state: &types::ElementState,
    error: &CommandError,
) -> Option<bool> {
    let target_missing = matches!(error.code, ErrorCode::TargetNotFound)
        || is_missing_css_node(error)
        || (matches!(error.code, ErrorCode::BrowserCommandFailed)
            && error
                .message
                .to_ascii_lowercase()
                .contains("target detached"));
    target_missing.then_some(matches!(state, types::ElementState::Detached))
}

fn is_closed_page_message(message: &str) -> bool {
    message.contains("receiver is gone")
        || message.contains("session closed")
        || message.contains("Session with given id not found")
        || message.contains("oneshot canceled")
        || message.contains(TARGET_GONE_MESSAGE)
}

fn wait_should_retry_replaced_context(error: &CommandError) -> bool {
    if error.code != ErrorCode::BrowserCommandFailed {
        return false;
    }
    let message = error.message.to_ascii_lowercase();
    message.contains("cannot find context with specified id")
        || message.contains("execution context was destroyed")
}

fn nonempty_field(value: &Option<String>) -> Option<&str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn is_page_scoped_css(css: &str) -> bool {
    matches!(css.to_ascii_lowercase().as_str(), "body" | "html" | ":root")
}

fn is_page_scoped_role(role: &str) -> bool {
    [
        "RootWebArea",
        "document",
        "main",
        "body",
        "application",
        "generic",
    ]
    .iter()
    .any(|name| role.eq_ignore_ascii_case(name))
}

/// a11y `RootWebArea` / `document` / bare landmarks, and `css: body|html|:root`,
/// mean "page text" — not a single resolvable node. Empty optional fields are
/// treated as absent so agents that send `""` still hit the body-text path.
fn is_page_scoped_text_target(target: &types::TargetSpec) -> bool {
    if nonempty_field(&target.test_id).is_some()
        || nonempty_field(&target.accessible_name).is_some()
        || nonempty_field(&target.label).is_some()
        || target.text.is_some()
        || !target.attributes.is_empty()
        || !target.shadow_path.is_empty()
        || target.ordinal.is_some()
    {
        return false;
    }
    let role = nonempty_field(&target.role);
    let css = nonempty_field(&target.css);
    match (role, css) {
        (Some(role), None) => is_page_scoped_role(role),
        (None, Some(css)) => is_page_scoped_css(css),
        (Some(role), Some(css)) => is_page_scoped_role(role) && is_page_scoped_css(css),
        (None, None) => false,
    }
}

async fn read_page_body_text(page: &Page) -> Result<String, CommandError> {
    if let Ok(result) = page
        .evaluate("document.body ? (document.body.innerText || '') : ''")
        .await
    {
        if let Ok(value) = result.into_value::<String>() {
            return Ok(value);
        }
    }
    let body = page.find_element("body").await.map_err(command_failed)?;
    Ok(body
        .inner_text()
        .await
        .map_err(command_failed)?
        .unwrap_or_default())
}

async fn read_page_scoped_text(
    browser: &Mutex<Option<Browser>>,
    page_id: &PageId,
    page: &Page,
    target: &TargetSpec,
) -> Result<String, CommandError> {
    if target.frame_path.is_empty() {
        return read_page_body_text(page).await;
    }
    let handle = browser
        .lock()
        .await
        .as_ref()
        .map(Browser::handle)
        .ok_or_else(closed_error)?;
    inspect_page_scoped_target(page_id, page, target, false, Some(&handle))
        .await
        .map(|(text, _, _)| text)
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

/// Maps a `page.goto` failure to a `CommandError`. A message naming
/// `net::ERR_ABORTED` is a navigation the browser aborted before it loaded,
/// such as a download response (Content-Disposition attachment or a
/// non-renderable type like application/octet-stream) or an explicitly
/// cancelled navigation; that case is not retryable and points the caller at
/// the download tools instead of surfacing the bare browser text. Every
/// other goto error keeps the existing mapping.
fn navigation_failed(url: &str, error: chromiumoxide::error::CdpError) -> CommandError {
    if error.to_string().contains("net::ERR_ABORTED") {
        return CommandError {
            code: ErrorCode::BrowserCommandFailed,
            message: format!(
                "navigation to {url} was aborted by the browser: the response is a download (Content-Disposition attachment / non-renderable type) or the navigation was cancelled; use download_url for files, or click_and_wait_for_download for in-page links"
            ),
            layer: ErrorLayer::Driver,
            retryable: false,
        };
    }
    driver_error(ErrorCode::BrowserCommandFailed, error)
}

/// Typing into a target that cannot accept text — disabled, readonly, a
/// fieldset-disabled ancestor, a non-typeable input type (hidden, button,
/// submit, reset, image, file), or a non-form/non-contenteditable element
/// such as `<h1>` — still dispatches key events. On macOS headless Chrome
/// that pins the browser process at 100% CPU permanently instead of
/// failing fast, so this must be checked before any key dispatch.
const EDITABLE_CONTROL_CHECK_JS: &str = "function() { const el = this; return !el.disabled && !el.closest('fieldset[disabled]') && ((el instanceof HTMLInputElement && !el.readOnly && !['hidden','button','submit','reset','image','file'].includes((el.type||'text').toLowerCase())) || (el instanceof HTMLTextAreaElement && !el.readOnly) || el instanceof HTMLSelectElement || el.isContentEditable); }";

/// Runs [`EDITABLE_CONTROL_CHECK_JS`] against the resolved target via
/// `DOM.resolveNode` + `Runtime.callFunctionOn` on its backend node id —
/// `ResolvedTarget`'s own JS-eval path is private to `targeting`, but its
/// `backend_node_id` accessor is `pub(crate)`, which is enough to scope the
/// same check through raw CDP from here.
async fn ensure_editable_control(
    page: &Page,
    resolved: &crate::targeting::ResolvedTarget,
) -> Result<(), CommandError> {
    let backend_id = resolved.backend_node_id(page).await?;
    let object_id = page
        .execute(
            chromiumoxide::cdp::browser_protocol::dom::ResolveNodeParams::builder()
                .backend_node_id(backend_id)
                .build(),
        )
        .await
        .map_err(command_failed)?
        .result
        .object
        .object_id
        .ok_or_else(|| driver_error(ErrorCode::TargetNotFound, "target has no live object"))?;
    let value = page
        .execute(
            chromiumoxide::cdp::js_protocol::runtime::CallFunctionOnParams::builder()
                .object_id(object_id)
                .function_declaration(EDITABLE_CONTROL_CHECK_JS)
                .return_by_value(true)
                .await_promise(false)
                .build()
                .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?,
        )
        .await
        .map_err(command_failed)?
        .result
        .result
        .value
        .ok_or_else(|| {
            driver_error(
                ErrorCode::BrowserCommandFailed,
                "editability check returned no value",
            )
        })?;
    let editable: bool = serde_json::from_value(value)
        .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
    if editable {
        return Ok(());
    }
    Err(CommandError {
        code: ErrorCode::InvalidRequest,
        message: "target is not an editable control (disabled, readonly, hidden, or not a text/select/checkable control)".into(),
        layer: ErrorLayer::Driver,
        retryable: false,
    })
}

fn screenshot_error(error: chromiumoxide::error::CdpError) -> CommandError {
    driver_error(ErrorCode::ScreenshotCaptureFailed, error)
}

/// Bounds a screenshot clip before it reaches CDP. An unbounded clip (e.g.
/// 50000x50000) sent to Chrome can kill the browser process outright, so
/// this must run before the CDP call rather than relying on the
/// `max_screenshot_dimension` cap `ArtifactStore` applies after capture.
fn validate_clip(x: f64, y: f64, width: f64, height: f64, max: u32) -> Result<(), CommandError> {
    if !x.is_finite()
        || !y.is_finite()
        || !width.is_finite()
        || !height.is_finite()
        || width <= 0.0
        || height <= 0.0
    {
        return Err(CommandError {
            code: ErrorCode::InvalidRequest,
            message: "screenshot clip must have finite positive dimensions".into(),
            layer: ErrorLayer::Driver,
            retryable: false,
        });
    }
    if x < 0.0 || y < 0.0 || width > max as f64 || height > max as f64 {
        return Err(CommandError {
            code: ErrorCode::InvalidRequest,
            message: format!(
                "screenshot clip exceeds the configured maximum dimension of {max}px or lies outside the viewport origin"
            ),
            layer: ErrorLayer::Driver,
            retryable: false,
        });
    }
    Ok(())
}

fn driver_error(code: ErrorCode, error: impl std::fmt::Display) -> CommandError {
    CommandError {
        code,
        message: error.to_string(),
        layer: ErrorLayer::Driver,
        retryable: driver_error_is_retryable(code),
    }
}

/// A driver failure is retryable unless the code names a condition a retry
/// cannot fix: a malformed request, a target that is not there, or a policy
/// refusal. Every other code (transport hiccups, launch failures, internal
/// errors, timeouts) may succeed on a fresh attempt.
fn driver_error_is_retryable(code: ErrorCode) -> bool {
    !matches!(
        code,
        ErrorCode::InvalidRequest
            | ErrorCode::VerificationFailed
            | ErrorCode::TargetNotFound
            | ErrorCode::TargetAmbiguous
            | ErrorCode::NotFound
            | ErrorCode::PolicyDenied
            | ErrorCode::FrameNotFound
    )
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

/// The worker's browser is gone or unreachable: dead command channel,
/// canceled oneshot, closed session, or an explicitly closed worker. Such a
/// worker can never serve another command, so callers may invalidate and
/// re-lease for a fresh browser instead of surfacing a dead-end failure.
pub fn is_dead_worker_error(error: &CommandError) -> bool {
    is_closed_page_message(&error.message) || error.message == "browser worker is closed"
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
fn count_a11y_nodes(nodes: &[types::AccessibilityNode]) -> usize {
    nodes
        .iter()
        .map(|node| 1 + count_a11y_nodes(&node.children))
        .sum()
}

fn a11y_contains_form_control(nodes: &[types::AccessibilityNode]) -> bool {
    nodes.iter().any(|node| {
        node.role.as_deref().is_some_and(|role| {
            matches!(
                role.to_ascii_lowercase().as_str(),
                "button"
                    | "checkbox"
                    | "combobox"
                    | "listbox"
                    | "radio"
                    | "searchbox"
                    | "slider"
                    | "spinbutton"
                    | "switch"
                    | "textbox"
            )
        }) || a11y_contains_form_control(&node.children)
    })
}

fn stamp_a11y_frame_path(
    nodes: &mut [types::AccessibilityNode],
    segment: &types::SemanticTargetSegment,
) {
    for node in nodes {
        if let Some(target) = &mut node.target {
            target.frame_path = vec![segment.clone()];
        }
        stamp_a11y_frame_path(&mut node.children, segment);
    }
}

/// Moves `frame_roots` under the `occurrence`-th main-tree `Iframe` node with
/// `name`; hands the roots back when no such node exists.
fn splice_under_iframe(
    nodes: &mut [types::AccessibilityNode],
    name: Option<&str>,
    occurrence: usize,
    frame_roots: Vec<types::AccessibilityNode>,
) -> Result<(), Vec<types::AccessibilityNode>> {
    fn go(
        nodes: &mut [types::AccessibilityNode],
        name: Option<&str>,
        seen: &mut usize,
        occurrence: usize,
        frame_roots: Vec<types::AccessibilityNode>,
    ) -> Result<(), Vec<types::AccessibilityNode>> {
        let mut frame_roots = frame_roots;
        for node in nodes.iter_mut() {
            if node.role.as_deref() == Some("Iframe") && node.name.as_deref() == name {
                if *seen == occurrence {
                    node.children = frame_roots;
                    return Ok(());
                }
                *seen += 1;
            }
            frame_roots = match go(&mut node.children, name, seen, occurrence, frame_roots) {
                Ok(()) => return Ok(()),
                Err(roots) => roots,
            };
        }
        Err(frame_roots)
    }
    go(nodes, name, &mut 0, occurrence, frame_roots)
}

/// Descends one level into same-process iframes (cap 8, shared node budget)
/// so the snapshot does not stop at `Iframe` nodes. Each frame's compacted
/// tree is spliced under its main-tree `Iframe` node and every in-frame
/// target is stamped with the role/name/ordinal hop that re-resolves the
/// iframe element at action time. Returns whether any frame was truncated.
async fn descend_a11y_iframes(
    page: &Page,
    nodes: &mut Vec<types::AccessibilityNode>,
    mut budget: usize,
) -> bool {
    let Ok(Some(main_frame)) = page.mainframe().await else {
        return false;
    };
    let Ok(iframes) = crate::targeting::main_frame_iframe_candidates(page).await else {
        return false;
    };
    if iframes.is_empty() {
        return false;
    }
    let Ok(frames) = page.frames().await else {
        return false;
    };
    // Frame <-> element identity, computed once for every candidate: name/src
    // are absent for a `srcdoc` iframe with no `name` attribute, so 2+ such
    // siblings cannot be told apart by the old heuristic. A candidate whose
    // backend node id lookup fails is skipped here and falls back to the
    // name/src heuristic per-frame below.
    let mut candidate_backend_ids: Vec<(
        chromiumoxide::cdp::browser_protocol::dom::BackendNodeId,
        &dom_engine::Candidate,
    )> = Vec::new();
    for candidate in &iframes {
        match crate::targeting::candidate_backend_node_id(
            page,
            &crate::targeting::LocatorScope::Context(None),
            &[],
            candidate,
        )
        .await
        {
            Ok(backend_node_id) => candidate_backend_ids.push((backend_node_id, candidate)),
            Err(error) => {
                tracing::debug!(
                    ?error,
                    "candidate backend node id lookup failed, iframe descent falls back to name/src match for this candidate"
                );
            }
        }
    }
    let mut truncated = false;
    for frame in frames {
        if budget == 0 {
            truncated = true;
            break;
        }
        let is_child = page
            .frame_parent(frame.clone())
            .await
            .ok()
            .flatten()
            .as_ref()
            == Some(&main_frame);
        if !is_child {
            continue;
        }
        let owner_backend_node_id = page
            .execute(
                chromiumoxide::cdp::browser_protocol::dom::GetFrameOwnerParams::new(frame.clone()),
            )
            .await
            .ok()
            .map(|owner| owner.result.backend_node_id);
        let candidate = match owner_backend_node_id.and_then(|backend_node_id| {
            candidate_backend_ids
                .iter()
                .find(|(id, _)| *id == backend_node_id)
                .map(|(_, candidate)| *candidate)
        }) {
            Some(candidate) => Some(candidate),
            None => {
                let frame_name = page.frame_name(frame.clone()).await.ok().flatten();
                let frame_url = page.frame_url(frame.clone()).await.ok().flatten();
                iframes
                    .iter()
                    .find(|candidate| {
                        frame_name
                            .as_ref()
                            .is_some_and(|name| candidate.attributes.get("name") == Some(name))
                            || frame_url.as_ref().is_some_and(|url| {
                                candidate
                                    .attributes
                                    .get("src")
                                    .is_some_and(|src| url == src || url.ends_with(src))
                            })
                    })
                    .or(if iframes.len() == 1 {
                        iframes.first()
                    } else {
                        None
                    })
            }
        };
        let Some(candidate) = candidate else {
            continue;
        };
        let Ok(tree) = page
            .execute(
                chromiumoxide::cdp::browser_protocol::accessibility::GetFullAxTreeParams::builder()
                    .frame_id(frame)
                    .build(),
            )
            .await
        else {
            continue;
        };
        let (mut frame_roots, frame_truncated) = compact_ax_tree(&tree.result.nodes, budget);
        truncated |= frame_truncated;
        // Two distinct counting domains: `splice_occurrence` finds the right
        // main-tree `Iframe` AX node (grouped by that node's own name, always
        // same-name); `hop_ordinal` is the resolver's own ranked index for
        // this hop (an unnamed hop matches every visible iframe candidate,
        // not just the unnamed ones, since `choose` drops the name filter
        // entirely once accessibleName normalizes to `None`).
        let same_name: Vec<&dom_engine::Candidate> = iframes
            .iter()
            .filter(|other| other.name == candidate.name)
            .collect();
        let splice_occurrence = same_name
            .iter()
            .position(|other| other.id == candidate.id)
            .unwrap_or(0);
        let visible_iframes: Vec<&dom_engine::Candidate> =
            iframes.iter().filter(|other| other.state.visible).collect();
        let hop_ordinal = iframe_hop_ordinal(&visible_iframes, candidate);
        let segment = types::SemanticTargetSegment {
            role: "iframe".into(),
            accessible_name: candidate.name.clone().unwrap_or_default(),
            ordinal: hop_ordinal,
        };
        stamp_a11y_frame_path(&mut frame_roots, &segment);
        budget = budget.saturating_sub(count_a11y_nodes(&frame_roots));
        match splice_under_iframe(
            nodes,
            candidate.name.as_deref(),
            splice_occurrence,
            frame_roots,
        ) {
            Ok(()) => {}
            Err(frame_roots) => {
                nodes.push(types::AccessibilityNode {
                    role: Some("Iframe".into()),
                    name: candidate.name.clone(),
                    children: frame_roots,
                    ..types::AccessibilityNode::default()
                });
            }
        }
    }
    truncated
}

/// The stamped hop's ordinal for an iframe candidate among the resolver's own
/// matching set: `choose` filters `collect_candidates` output by role and (if
/// set) accessibleName, over attached+visible candidates, then ranks by score
/// and DOM order. A named candidate's matching set is every visible iframe
/// with that name; an unnamed candidate's is every visible iframe, since a
/// normalized (`None`) accessibleName applies no name filter at all. `None`
/// when that set has at most one element, matching the pre-existing rule that
/// an unambiguous hop carries no ordinal.
fn iframe_hop_ordinal(
    visible_iframes: &[&dom_engine::Candidate],
    candidate: &dom_engine::Candidate,
) -> Option<usize> {
    let matching: Vec<&&dom_engine::Candidate> = visible_iframes
        .iter()
        .filter(|other| candidate.name.is_none() || other.name == candidate.name)
        .collect();
    (matching.len() > 1).then(|| {
        matching
            .iter()
            .position(|other| other.id == candidate.id)
            .unwrap_or(0)
    })
}

fn compact_ax_tree(
    raw: &[chromiumoxide::cdp::browser_protocol::accessibility::AxNode],
    max_nodes: usize,
) -> (Vec<types::AccessibilityNode>, bool) {
    compact_ax_tree_from(raw, max_nodes, None)
}

/// `forced_root` scopes the output to the subtree rooted at that AX node id
/// (a scoped `a11y_snapshot`); `None` compacts the whole tree.
fn compact_ax_tree_from(
    raw: &[chromiumoxide::cdp::browser_protocol::accessibility::AxNode],
    max_nodes: usize,
    forced_root: Option<&str>,
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

    fn name_text(
        value: &Option<chromiumoxide::cdp::browser_protocol::accessibility::AxValue>,
    ) -> Option<String> {
        text(value)
            .map(|value| value.trim().to_owned())
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
        let name = name_text(&node.name);
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
        let role = text(&node.role);
        let name = name_text(&node.name);
        // InlineTextBox leaves carry the same text as their StaticText
        // parent — pure payload duplication. Skip them regardless of name.
        // Unlabeled generic wrappers are skipped too; keep their children by
        // re-parenting.
        let emits = !node.ignored
            && role.as_deref() != Some("InlineTextBox")
            && !(matches!(role.as_deref(), None | Some("generic" | "none")) && name.is_none());
        // Budget is reserved before descending so deep leaves can't starve
        // ancestors: without this, a node's own slot could be consumed by
        // its children first and the node itself would never emit.
        if emits {
            if *budget == 0 {
                return None;
            }
            *budget -= 1;
        }
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
        if !emits {
            return (!children.is_empty()).then_some(types::AccessibilityNode {
                children,
                ..types::AccessibilityNode::default()
            });
        }
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
        // Only links: the AX `url` property also lands on the root web
        // area, where it is the document URL — which for a data: page
        // embeds the whole document, secrets included.
        let url = (role.as_deref() == Some("link"))
            .then(|| property_text(node, "url"))
            .flatten();
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
            url,
            value_min: property_text(node, "valuemin"),
            value_max: property_text(node, "valuemax"),
            children,
        })
    }

    fn lift(node: types::AccessibilityNode, lifted: &mut Vec<types::AccessibilityNode>) {
        lifted.push(node);
    }

    let roots: Vec<&str> = match forced_root {
        Some(root) => vec![root],
        None => raw
            .iter()
            .filter(|node| {
                node.parent_id
                    .as_ref()
                    .is_none_or(|parent| !by_id.contains_key(parent.as_ref()))
            })
            .map(|node| node.node_id.as_ref())
            .collect(),
    };
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
    let truncated = budget == 0 && (forced_root.is_some() || raw.len() > max_nodes);
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

fn chromium_modifier_bits(modifiers: &[types::ClickModifier]) -> i64 {
    modifiers.iter().fold(0, |bits, modifier| {
        bits | match modifier {
            types::ClickModifier::Alt => 1,
            types::ClickModifier::Ctrl => 2,
            types::ClickModifier::Meta => 4,
            types::ClickModifier::Shift => 8,
        }
    })
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
    use super::bound_observed;

    /// A wait observation is truncated on a character boundary.
    ///
    /// Byte-index truncation panics inside a multi-byte codepoint, which is
    /// how extraction used to die on any non-ASCII page.
    #[test]
    fn a_wait_observation_is_bounded_on_a_character_boundary() {
        let ascii = "a".repeat(types::MAX_WAIT_OBSERVED_CHARS + 50);
        assert_eq!(
            bound_observed(&ascii).chars().count(),
            types::MAX_WAIT_OBSERVED_CHARS
        );

        // Every char is 3 bytes, so a byte-index cut would land mid-codepoint.
        let multibyte = "\u{6f22}".repeat(types::MAX_WAIT_OBSERVED_CHARS + 50);
        let bounded = bound_observed(&multibyte);
        assert_eq!(bounded.chars().count(), types::MAX_WAIT_OBSERVED_CHARS);
        assert!(multibyte.starts_with(&bounded));

        let short = "https://example.com/order/confirmed";
        assert_eq!(bound_observed(short), short);
    }

    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use chromiumoxide::cdp::browser_protocol::network::{
        Cookie, CookiePriority, CookieSourceScheme,
    };

    use super::{
        apply_state_commit, clamp_js_timeout_ms, compact_ax_tree, driver_error_is_retryable,
        element_wait_missing_observation, ensure_automatic_download_modifier_support,
        iframe_hop_ordinal, is_closed_page_message, is_dead_worker_error, is_missing_css_node,
        should_retry_plain_click_target_drift, snapshot_cookie, text_matches,
        unscoped_css_wait_selector, validate_clip, wait_should_retry_replaced_context,
        ChromiumWorker, HttpBridgeState, EDITABLE_CONTROL_CHECK_JS, TARGET_GONE_MESSAGE,
    };
    use types::{
        ClickModifier, CommandError, ErrorCode, ErrorLayer, PageId, SessionId, TargetSpec,
        TextMatch, WorkerId,
    };

    #[test]
    fn editable_control_check_js_covers_every_required_guard() {
        assert!(EDITABLE_CONTROL_CHECK_JS.contains("!el.disabled"));
        assert!(EDITABLE_CONTROL_CHECK_JS.contains("el.closest('fieldset[disabled]')"));
        assert!(EDITABLE_CONTROL_CHECK_JS.contains("HTMLInputElement"));
        assert!(EDITABLE_CONTROL_CHECK_JS.contains("!el.readOnly"));
        assert!(EDITABLE_CONTROL_CHECK_JS.contains("'hidden'"));
        assert!(EDITABLE_CONTROL_CHECK_JS.contains("'file'"));
        assert!(EDITABLE_CONTROL_CHECK_JS.contains("HTMLTextAreaElement"));
        assert!(EDITABLE_CONTROL_CHECK_JS.contains("HTMLSelectElement"));
        assert!(EDITABLE_CONTROL_CHECK_JS.contains("el.isContentEditable"));
    }

    #[test]
    fn driver_error_retryable_matches_the_failure_class() {
        let not_retryable = [
            ErrorCode::InvalidRequest,
            ErrorCode::VerificationFailed,
            ErrorCode::TargetNotFound,
            ErrorCode::TargetAmbiguous,
            ErrorCode::NotFound,
            ErrorCode::PolicyDenied,
            ErrorCode::FrameNotFound,
        ];
        for code in not_retryable {
            assert!(
                !driver_error_is_retryable(code),
                "{code:?} must not be retryable"
            );
        }

        let retryable = [
            ErrorCode::BrowserCommandFailed,
            ErrorCode::BrowserLaunchFailed,
            ErrorCode::Internal,
            ErrorCode::ScreenshotCaptureFailed,
            ErrorCode::TargetDetached,
            ErrorCode::DeadlineExceeded,
        ];
        for code in retryable {
            assert!(
                driver_error_is_retryable(code),
                "{code:?} must be retryable"
            );
        }
    }

    #[test]
    fn validate_clip_rejects_dimensions_over_the_configured_maximum() {
        let error = validate_clip(-10.0, -10.0, 50_000.0, 50_000.0, 16_384)
            .expect_err("clip exceeding the maximum dimension must be rejected");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(!error.retryable);
    }

    #[test]
    fn validate_clip_rejects_negative_origin() {
        let error = validate_clip(-1.0, 0.0, 100.0, 100.0, 16_384)
            .expect_err("clip with a negative origin must be rejected");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(!error.retryable);
    }

    #[test]
    fn validate_clip_accepts_dimensions_within_the_maximum() {
        validate_clip(0.0, 0.0, 100.0, 100.0, 16_384).expect("clip within bounds must be accepted");
    }

    #[test]
    fn validate_clip_rejects_non_finite_dimensions() {
        let error = validate_clip(0.0, 0.0, f64::NAN, 100.0, 16_384)
            .expect_err("a non-finite dimension must be rejected");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(!error.retryable);
    }

    #[test]
    fn modified_download_clicks_fail_instead_of_dropping_native_modifiers() {
        assert!(ensure_automatic_download_modifier_support(&[]).is_ok());
        let error = ensure_automatic_download_modifier_support(&[ClickModifier::Shift])
            .expect_err("automatic download capture cannot preserve click modifiers");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(!error.retryable);
        assert!(error.message.contains("modifiers"));
    }

    #[test]
    fn non_boundary_click_retries_bounded_predispatch_stale_targets_only() {
        let stale = CommandError {
            code: ErrorCode::TargetNotFound,
            message: "stale".into(),
            layer: ErrorLayer::Driver,
            retryable: false,
        };
        let other = CommandError {
            code: ErrorCode::BrowserCommandFailed,
            message: "other".into(),
            layer: ErrorLayer::Driver,
            retryable: false,
        };

        assert!(should_retry_plain_click_target_drift(false, 0, &stale));
        assert!(should_retry_plain_click_target_drift(false, 1, &stale));
        assert!(should_retry_plain_click_target_drift(false, 2, &stale));
        assert!(!should_retry_plain_click_target_drift(false, 3, &stale));
        assert!(!should_retry_plain_click_target_drift(true, 0, &stale));
        assert!(!should_retry_plain_click_target_drift(false, 0, &other));
    }

    fn chromium_worker_without_browser(root: &std::path::Path) -> ChromiumWorker {
        let behavioral = super::BehavioralConfig::default().sanitize();
        let fingerprint = super::FingerprintConfig::default();
        ChromiumWorker {
            id: WorkerId::new(),
            profile_dir: root.join("profile"),
            pid_registry_path: None,
            debug_ws_url: None,
            upload_roots: Vec::new(),
            download_dir: root.join("downloads"),
            session_id: SessionId::new(),
            artifacts: super::ArtifactStore::new(root.join("artifacts"), 1024, 1024),
            max_screenshot_dimension: 1024,
            max_js_result_bytes: 1024,
            max_js_timeout_ms: 1_000,
            browser: super::Mutex::new(None),
            pages: super::Mutex::new(super::HashMap::new()),
            closed_targets: super::Mutex::new(super::HashSet::new()),
            network_trackers: super::Mutex::new(super::HashMap::new()),
            har_recorders: super::Mutex::new(super::HashMap::new()),
            har_tasks: super::Mutex::new(super::HashMap::new()),
            pending_dialogs: super::Arc::new(super::Mutex::new(super::HashMap::new())),
            dialog_tasks: super::Mutex::new(super::HashMap::new()),
            http_state: super::Mutex::new(HttpBridgeState::default()),
            handler_task: super::Mutex::new(None),
            fingerprint: super::Mutex::new(fingerprint.clone()),
            fingerprint_enabled: super::AtomicBool::new(fingerprint.enabled),
            fingerprint_plan: super::Mutex::new(None),
            humanization_enabled: super::AtomicBool::new(false),
            typing_simulator: super::TypingSimulator::new(behavioral.typing.clone()),
            mouse_simulator: super::BezierMouseSimulator::new(behavioral.mouse.clone()),
            session_jitter: behavioral.session_jitter,
            session_random: super::Mutex::new(super::SessionRandom::new(1)),
        }
    }

    #[tokio::test]
    async fn unregistering_a_page_invalidates_its_cached_har_collector() {
        let temp = tempfile::tempdir().expect("temporary worker root");
        let worker = chromium_worker_without_browser(temp.path());
        let page_id = PageId::new();
        worker.har_recorders.lock().await.insert(
            page_id.clone(),
            std::sync::Arc::new(crate::HarRecorder::default()),
        );
        worker
            .har_tasks
            .lock()
            .await
            .insert(page_id.clone(), tokio::spawn(std::future::pending::<()>()));

        worker.unregister_page(&page_id).await;

        assert!(!worker.har_recorders.lock().await.contains_key(&page_id));
        assert!(!worker.har_tasks.lock().await.contains_key(&page_id));
    }

    #[tokio::test]
    async fn recovery_preserves_entries_while_detaching_the_old_har_task() {
        let temp = tempfile::tempdir().expect("temporary worker root");
        let worker = chromium_worker_without_browser(temp.path());
        let page_id = PageId::new();
        let recorder = std::sync::Arc::new(crate::HarRecorder::default());
        recorder
            .record(crate::HarEntry {
                url: "https://example.test/before-crash".into(),
                method: "GET".into(),
                status: Some(200),
                status_text: Some("OK".into()),
                redirect_url: None,
                started_unix_ms: 1.0,
                elapsed_ms: Some(2.0),
                transfer_bytes: Some(3),
                mime_type: Some("text/plain".into()),
                error_text: None,
            })
            .await;
        worker
            .har_recorders
            .lock()
            .await
            .insert(page_id.clone(), recorder.clone());
        worker
            .har_tasks
            .lock()
            .await
            .insert(page_id.clone(), tokio::spawn(std::future::pending::<()>()));

        let (_, recovered) = worker.unregister_page_state(&page_id, true).await;

        let recovered = recovered.expect("recovery retains the recorder");
        assert!(std::sync::Arc::ptr_eq(&recorder, &recovered));
        let entries = recovered.take(false).await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].url, "https://example.test/before-crash");
        assert!(!worker.har_tasks.lock().await.contains_key(&page_id));
    }

    #[test]
    fn element_wait_recognizes_an_unscoped_css_query() {
        let target = TargetSpec {
            css: Some("a[href='/customers/cus_atlas']".into()),
            ..TargetSpec::default()
        };

        assert_eq!(
            unscoped_css_wait_selector(&target),
            Some("a[href='/customers/cus_atlas']")
        );
    }

    #[test]
    fn element_wait_treats_chromes_missing_node_as_not_yet_present() {
        let error = types::CommandError {
            code: ErrorCode::BrowserCommandFailed,
            message: "Error -32000: Could not find node with given id".into(),
            layer: types::ErrorLayer::Driver,
            retryable: false,
        };

        assert!(is_missing_css_node(&error));
    }

    #[test]
    fn detached_element_wait_accepts_target_loss_between_resolution_and_probe() {
        let error = types::CommandError {
            code: ErrorCode::BrowserCommandFailed,
            message: "ExceptionDetails: Error: target detached".into(),
            layer: types::ErrorLayer::Driver,
            retryable: false,
        };

        assert_eq!(
            element_wait_missing_observation(&types::ElementState::Detached, &error),
            Some(true)
        );
        assert_eq!(
            element_wait_missing_observation(&types::ElementState::Visible, &error),
            Some(false)
        );
    }

    #[test]
    fn element_wait_does_not_swallow_page_target_loss() {
        let error = types::CommandError {
            code: ErrorCode::TargetDetached,
            message: "the browser target is gone (crashed or closed)".into(),
            layer: types::ErrorLayer::Driver,
            retryable: true,
        };

        assert_eq!(
            element_wait_missing_observation(&types::ElementState::Detached, &error),
            None
        );
        assert_eq!(
            element_wait_missing_observation(&types::ElementState::Visible, &error),
            None
        );
    }

    #[test]
    fn waits_retry_only_execution_context_replacement() {
        let replaced = types::CommandError {
            code: ErrorCode::BrowserCommandFailed,
            message: "Error -32000: Cannot find context with specified id".into(),
            layer: types::ErrorLayer::Driver,
            retryable: false,
        };
        assert!(wait_should_retry_replaced_context(&replaced));

        let target_gone = types::CommandError {
            code: ErrorCode::TargetDetached,
            message: "the browser target is gone (crashed or closed)".into(),
            layer: types::ErrorLayer::Driver,
            retryable: true,
        };
        assert!(!wait_should_retry_replaced_context(&target_gone));

        let unrelated = types::CommandError {
            code: ErrorCode::BrowserCommandFailed,
            message: "permission denied".into(),
            layer: types::ErrorLayer::Driver,
            retryable: false,
        };
        assert!(!wait_should_retry_replaced_context(&unrelated));
    }

    #[test]
    fn list_pages_recognizes_a_window_closed_by_the_site() {
        assert!(is_closed_page_message(
            "send failed because receiver is gone"
        ));
        assert!(is_closed_page_message("oneshot canceled"));
        assert!(!is_closed_page_message(
            "connection temporarily unavailable"
        ));
    }

    #[test]
    fn dead_target_error_rewritten_by_targeting_triggers_the_revive_path() {
        let rewritten = CommandError {
            code: ErrorCode::TargetDetached,
            message: TARGET_GONE_MESSAGE.into(),
            layer: ErrorLayer::Driver,
            retryable: true,
        };
        assert!(is_dead_worker_error(&rewritten));

        let stale_element = CommandError {
            code: ErrorCode::TargetDetached,
            message: "target has no live object".into(),
            layer: ErrorLayer::Driver,
            retryable: false,
        };
        assert!(!is_dead_worker_error(&stale_element));
    }

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

    fn iframe_candidate(id: &str, name: Option<&str>, visible: bool) -> dom_engine::Candidate {
        dom_engine::Candidate {
            id: id.into(),
            css: None,
            test_id: None,
            role: Some("iframe".into()),
            name: name.map(str::to_string),
            label: None,
            text: String::new(),
            attributes: BTreeMap::new(),
            state: dom_engine::CandidateState {
                attached: true,
                visible,
                enabled: true,
            },
            frame_path: Vec::new(),
        }
    }

    #[test]
    fn iframe_hop_ordinal_for_unnamed_iframe_counts_every_visible_iframe() {
        // A named sibling sits between the two unnamed candidates; an
        // unnamed hop's ordinal must count through it (position 2 for
        // `two`), not group only with the other unnamed candidate (which
        // would give position 1).
        let one = iframe_candidate("one", None, true);
        let named = iframe_candidate("named", Some("Named"), true);
        let two = iframe_candidate("two", None, true);
        let visible = [&one, &named, &two];

        assert_eq!(iframe_hop_ordinal(&visible, &one), Some(0));
        assert_eq!(iframe_hop_ordinal(&visible, &two), Some(2));
    }

    #[test]
    fn iframe_hop_ordinal_for_named_iframe_counts_only_same_name_iframes() {
        let first_ada = iframe_candidate("a", Some("Ada"), true);
        let unnamed = iframe_candidate("u", None, true);
        let second_ada = iframe_candidate("b", Some("Ada"), true);
        let visible = [&first_ada, &unnamed, &second_ada];

        assert_eq!(iframe_hop_ordinal(&visible, &first_ada), Some(0));
        assert_eq!(iframe_hop_ordinal(&visible, &second_ada), Some(1));
    }

    #[test]
    fn iframe_hop_ordinal_is_none_for_a_single_match() {
        let only = iframe_candidate("only", None, true);
        let visible = [&only];
        assert_eq!(iframe_hop_ordinal(&visible, &only), None);
    }

    #[test]
    fn accessibility_snapshot_trims_surrounding_whitespace_from_the_name() {
        let raw: Vec<chromiumoxide::cdp::browser_protocol::accessibility::AxNode> =
            serde_json::from_value(serde_json::json!([{
                "nodeId": "root",
                "ignored": true,
                "childIds": ["1"]
            }, {
                "nodeId": "1",
                "ignored": false,
                "parentId": "root",
                "role": {"type": "role", "value": "textbox"},
                "name": {"type": "computedString", "value": "Name "}
            }]))
            .expect("valid CDP AX fixture");

        let (nodes, truncated) = compact_ax_tree(&raw, 10);
        assert!(!truncated);
        assert_eq!(nodes[0].name.as_deref(), Some("Name"));
        assert_eq!(nodes[0].target.as_ref().unwrap().accessible_name, "Name");
    }

    #[test]
    fn accessibility_snapshot_drops_named_inline_text_box_leaves() {
        let raw: Vec<chromiumoxide::cdp::browser_protocol::accessibility::AxNode> =
            serde_json::from_value(serde_json::json!([{
                "nodeId": "root",
                "ignored": true,
                "childIds": ["1"]
            }, {
                "nodeId": "1",
                "ignored": false,
                "parentId": "root",
                "role": {"type": "role", "value": "StaticText"},
                "name": {"type": "computedString", "value": "Priority saved"},
                "childIds": ["2"]
            }, {
                "nodeId": "2",
                "ignored": false,
                "parentId": "1",
                "role": {"type": "role", "value": "InlineTextBox"},
                "name": {"type": "computedString", "value": "Priority saved"}
            }]))
            .expect("valid CDP AX fixture");

        let (nodes, truncated) = compact_ax_tree(&raw, 10);
        assert!(!truncated);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].role.as_deref(), Some("StaticText"));
        assert!(
            nodes[0].children.is_empty(),
            "InlineTextBox leaves must be pruned even when named"
        );
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

    /// Budget must be reserved for a node before its children are visited,
    /// or a deep subtree of leaves can consume the whole budget before any
    /// ancestor — including the root — gets a turn, leaving the snapshot
    /// with zero nodes no matter how large the cap is raised.
    #[test]
    fn compact_ax_tree_reserves_budget_before_descending_into_children() {
        let mut item_ids = Vec::new();
        let mut items = Vec::new();
        for i in 0..300 {
            let item_id = format!("item{i}");
            let text_id = format!("{item_id}-text");
            items.push(serde_json::json!({
                "nodeId": item_id,
                "ignored": false,
                "parentId": "list",
                "role": {"type": "role", "value": "listitem"},
                "childIds": [text_id]
            }));
            items.push(serde_json::json!({
                "nodeId": text_id,
                "ignored": false,
                "parentId": item_id,
                "role": {"type": "role", "value": "StaticText"},
                "name": {"type": "computedString", "value": format!("Item {i}")}
            }));
            item_ids.push(item_id);
        }

        let mut all = vec![
            serde_json::json!({
                "nodeId": "root",
                "ignored": false,
                "role": {"type": "role", "value": "RootWebArea"},
                "childIds": ["heading", "list"]
            }),
            serde_json::json!({
                "nodeId": "heading",
                "ignored": false,
                "parentId": "root",
                "role": {"type": "role", "value": "heading"},
                "name": {"type": "computedString", "value": "Fixture page"}
            }),
            serde_json::json!({
                "nodeId": "list",
                "ignored": false,
                "parentId": "root",
                "role": {"type": "role", "value": "list"},
                "childIds": item_ids
            }),
        ];
        all.extend(items);

        let raw: Vec<chromiumoxide::cdp::browser_protocol::accessibility::AxNode> =
            serde_json::from_value(serde_json::Value::Array(all)).expect("valid CDP AX fixture");

        let (nodes, truncated) = compact_ax_tree(&raw, 10);
        assert_eq!(nodes.len(), 1, "root must always be emitted");
        assert_eq!(super::count_a11y_nodes(&nodes), 10);
        assert!(truncated);

        let (nodes, truncated) = compact_ax_tree(&raw, 1);
        assert_eq!(nodes.len(), 1, "root must always be emitted");
        assert_eq!(super::count_a11y_nodes(&nodes), 1);
        assert!(truncated);

        let (nodes, truncated) = compact_ax_tree(&raw, 5000);
        assert_eq!(super::count_a11y_nodes(&nodes), raw.len());
        assert!(!truncated);
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

    #[test]
    fn document_web_area_roles_are_recognized() {
        assert!(super::is_page_scoped_text_target(&types::TargetSpec {
            role: Some("RootWebArea".into()),
            ..types::TargetSpec::default()
        }));
        assert!(super::is_page_scoped_text_target(&types::TargetSpec {
            role: Some("document".into()),
            ..types::TargetSpec::default()
        }));
        assert!(super::is_page_scoped_text_target(&types::TargetSpec {
            role: Some("main".into()),
            ..types::TargetSpec::default()
        }));
        assert!(super::is_page_scoped_text_target(&types::TargetSpec {
            css: Some("body".into()),
            ..types::TargetSpec::default()
        }));
        assert!(super::is_page_scoped_text_target(&types::TargetSpec {
            role: Some("main".into()),
            css: Some("body".into()),
            ..types::TargetSpec::default()
        }));
        assert!(super::is_page_scoped_text_target(&types::TargetSpec {
            role: Some("main".into()),
            css: Some("".into()),
            accessible_name: Some("".into()),
            ..types::TargetSpec::default()
        }));
        assert!(!super::is_page_scoped_text_target(&types::TargetSpec {
            role: Some("main".into()),
            css: Some("#content".into()),
            ..types::TargetSpec::default()
        }));
        assert!(!super::is_page_scoped_text_target(&types::TargetSpec {
            role: Some("status".into()),
            ..types::TargetSpec::default()
        }));
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
