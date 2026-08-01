use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use std::time::Duration;
use std::time::Instant;

use artifact_store::ArtifactStore;
use async_trait::async_trait;
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
use futures::StreamExt;
use network_engine::state::{
    HttpCookie, HttpCookiePartitionKey, HttpStateSnapshot, ResponseStateDelta,
};
use sha2::{Digest, Sha256};
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
}

impl ChromiumWorkerFactory {
    /// Constructs a factory and reaps any Chrome processes orphaned by a
    /// previous instance of this runtime (see `reap_orphaned_chrome_processes`
    /// doc comment below for why this is necessary and safe). The reap only
    /// runs once per process: the shared registry directory is also used by
    /// every other same-process worker, so sweeping it again later could
    /// mistake a live sibling worker's still-registered PID for an orphan.
    pub fn new(config: BrowserConfig) -> Self {
        let pid_registry_dir = default_pid_registry_dir();
        reap_orphaned_chrome_processes_once(&pid_registry_dir);
        Self {
            config,
            pid_registry_dir,
        }
    }

    /// Same as `new`, but with an explicit PID registry location instead of
    /// the shared OS temp directory, reaped unconditionally on every call
    /// (not gated behind the process-wide once guard). Safe as long as the
    /// directory is exclusive to this factory — which is always true for an
    /// isolated per-test tempdir — since nothing else can register a live
    /// sibling PID into it. Exists so tests can exercise orphan reaping
    /// without touching, or being affected by, every other Chromium worker
    /// on the machine.
    pub fn with_pid_registry_dir(config: BrowserConfig, pid_registry_dir: PathBuf) -> Self {
        reap_orphaned_chrome_processes(&pid_registry_dir);
        Self {
            config,
            pid_registry_dir,
        }
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
            .launch_timeout(Duration::from_secs(20));
        if let Some(executable) = &self.config.executable {
            builder = builder.chrome_executable(executable);
        }
        if !self.config.headless {
            builder = builder.with_head();
        }
        let config = builder
            .build()
            .map_err(|error| driver_error(ErrorCode::BrowserLaunchFailed, error))?;
        let (mut browser, mut handler) = Browser::launch(config)
            .await
            .map_err(|error| driver_error(ErrorCode::BrowserLaunchFailed, error))?;
        let worker_id = WorkerId::new();
        // Best-effort: if we can't read the child PID or can't write the
        // registry file, launch still proceeds — this is a self-healing
        // backstop for *future* runs, not a launch precondition.
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
            http_state: Mutex::new(HttpBridgeState::default()),
            handler_task: Mutex::new(Some(handler_task)),
        }))
    }
}

/// Shared, machine-wide directory Chromium PID registrations live in when a
/// caller doesn't provide an explicit one (`ChromiumWorkerFactory::new`).
/// This directory, and everything below, is Chromium-specific plumbing on
/// top of the engine-agnostic `process_registry` module — see that module's
/// doc comment for why the underlying mechanism isn't Chrome-specific.
fn default_pid_registry_dir() -> PathBuf {
    std::env::temp_dir().join("bobby-browser-chromium-workers")
}

/// Records `pid` as the Chrome process backing `worker_id`, so a *future*
/// process (the next test run, or the runtime restarting after a crash) can
/// recognize and reap it if this process never gets a chance to call
/// `close`/`terminate` on it. Returns `None` on any I/O failure — recording
/// is best-effort and never blocks a launch.
fn register_chrome_pid(registry_dir: &Path, worker_id: &WorkerId, pid: u32) -> Option<PathBuf> {
    process_registry::register_pid(registry_dir, &worker_id.0.to_string(), pid)
}

fn unregister_chrome_pid(path: &Path) {
    process_registry::unregister_pid(path);
}

static ORPHAN_REAP_ONCE: Once = Once::new();

/// Runs `reap_orphaned_chrome_processes` at most once per process. The
/// registry directory is shared by every worker this process ever launches;
/// sweeping it again after this process's own workers have registered would
/// risk mistaking a live sibling worker for an orphan left by someone else.
fn reap_orphaned_chrome_processes_once(registry_dir: &Path) {
    let registry_dir = registry_dir.to_path_buf();
    ORPHAN_REAP_ONCE.call_once(|| reap_orphaned_chrome_processes(&registry_dir));
}

/// Chrome-specific entry point into `process_registry::reap_orphaned_processes`:
/// every registry entry is verified to actually be a Chrome/Chromium process
/// (`is_running_chrome_process`) before it's killed, so a PID that has since
/// been reused by an unrelated process is never touched.
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
    http_state: Mutex<HttpBridgeState>,
    handler_task: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Default)]
struct HttpBridgeState {
    version: u64,
    cache_validators: BTreeMap<String, String>,
}

impl ChromiumWorker {
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
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
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
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
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
                    page,
                    command.selector.as_deref().unwrap_or(""),
                    command.target.as_ref(),
                )
                .await?;
            let text = match resolved.value(page).await {
                Ok(Some(value)) if !value.is_empty() => value,
                _ => resolved.inner_text(page).await?.unwrap_or_default(),
            };
            let html = if command.include_html {
                Some(resolved.outer_html(page).await?.unwrap_or_default())
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
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
        let resolved = self
            .resolve_target(page_id, page, &command.selector, command.target.as_ref())
            .await?;
        let text = resolved.inner_text(page).await.ok().flatten();
        resolved.click(page).await?;
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
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
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
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
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
            .resolve_target(page_id, page, &command.selector, command.target.as_ref())
            .await?;
        let observed = if resolved.is_checkable(page).await? {
            let checked = command.value.parse::<bool>().map_err(|_| {
                driver_error(
                    ErrorCode::InvalidRequest,
                    "checkable controls require a boolean value",
                )
            })?;
            resolved.set_checked(page, checked).await?.to_string()
        } else if resolved.is_select(page).await? {
            resolved.select_option(page, &command.value).await?
        } else {
            resolved
                .type_text(page, &command.value, command.clear_first)
                .await?;
            resolved.value(page).await?.unwrap_or_default()
        };
        let validity = resolved.form_control_validity(page).await?;
        Ok(vec![
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
        ])
    }

    async fn upload_files(
        &self,
        page_id: &PageId,
        command: &UploadFilesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let requested = command.paths.iter().map(PathBuf::from).collect::<Vec<_>>();
        let paths = resolve_upload_paths(&self.upload_roots, &requested)?;
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
        let resolved = self
            .resolve_target(page_id, page, &command.selector, command.target.as_ref())
            .await?;
        let path_strings = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        resolved.set_files(page, path_strings.clone()).await?;
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
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
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
            .resolve_target(page_id, page, "", Some(&target))
            .await?;
        match &command.action {
            ControlAction::SetText { value } => resolved.type_text(page, value, true).await?,
            ControlAction::SetChecked { checked } => {
                resolved.set_checked(page, *checked).await?;
            }
            ControlAction::SelectOne { value } => {
                resolved.select_option(page, value).await?;
            }
            ControlAction::SelectMany { values } => {
                resolved.select_options(page, values).await?;
            }
            ControlAction::SetFiles { paths } => {
                let requested = paths.iter().map(PathBuf::from).collect::<Vec<_>>();
                let paths = resolve_upload_paths(&self.upload_roots, &requested)?
                    .into_iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect();
                resolved.set_files(page, paths).await?;
            }
            ControlAction::Clear => {
                if before_control.control_kind == types::FormControlKind::File {
                    resolved.set_files(page, Vec::new()).await?;
                } else {
                    resolved.clear_control(page).await?;
                }
            }
            ControlAction::Activate => resolved.click(page).await?,
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
        let browser = self.browser.lock().await;
        let browser = browser.as_ref().ok_or_else(closed_error)?;
        let page = browser
            .new_page(command.url.as_deref().unwrap_or("about:blank"))
            .await
            .map_err(command_failed)?;
        let evidence = page_evidence(page_id.clone(), &page).await?;
        self.register_page(page_id, page).await?;
        Ok(vec![Evidence::Page {
            page_id: evidence.page_id,
            url: evidence.url,
            title: evidence.title,
        }])
    }

    async fn list_pages(&self, _command: &ListPagesCommand) -> Result<Vec<Evidence>, CommandError> {
        let pages = self.pages.lock().await;
        let mut listed = Vec::with_capacity(pages.len());
        for (page_id, page) in pages.iter() {
            listed.push(page_evidence(page_id.clone(), page).await?);
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

    async fn emulate(
        &self,
        page_id: &PageId,
        command: &types::EmulateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
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
            page.execute(params).await.map_err(command_failed)?;
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
            page.execute(params).await.map_err(command_failed)?;
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
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
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
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
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
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
        let mut params = chromiumoxide::cdp::browser_protocol::network::GetCookiesParams::default();
        if !command.urls.is_empty() {
            params.urls = Some(command.urls.clone());
        }
        let result = page.execute(params).await.map_err(command_failed)?.result;
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
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
        if command.cookies.len() > 128 {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                "cookie set exceeds the 128-cookie bound",
            ));
        }
        let params = chromiumoxide::cdp::browser_protocol::network::SetCookiesParams {
            cookies: command.cookies.iter().map(set_cookie_param).collect(),
        };
        page.execute(params).await.map_err(command_failed)?;
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
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
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
            page.execute(params).await.map_err(command_failed)?;
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
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
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
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
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
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
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
        let pages = self.pages.lock().await;
        let page = pages.get(&command.page_id).ok_or_else(page_missing)?;
        page.activate().await.map_err(command_failed)?;
        let evidence = page_evidence(command.page_id.clone(), page).await?;
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
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
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
            .resolve_target(page_id, page, &command.selector, command.target.as_ref())
            .await?;
        resolved.click_js(page).await?;
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
            let pages = self.pages.lock().await;
            let page = pages.get(page_id).ok_or_else(page_missing)?;
            let (satisfied, excluded_classes) = wait_condition_satisfied(
                &self.browser,
                page_id,
                page,
                tracker.as_deref(),
                &command.condition,
                &mut quiet_since,
            )
            .await?;
            drop(pages);
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
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
        let mut browser = self.browser.lock().await;
        let browser = browser.as_mut().ok_or_else(closed_error)?;
        gather_candidates(page, target, Some(browser)).await
    }

    async fn capture_screenshot(
        &self,
        page_id: &PageId,
        command: &CaptureScreenshotCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
        let (bytes, resolution) = match &command.mode {
            ScreenshotMode::Viewport => (
                page.screenshot(
                    ScreenshotParams::builder()
                        .format(CaptureScreenshotFormat::Png)
                        .build(),
                )
                .await
                .map_err(screenshot_error)?,
                None,
            ),
            ScreenshotMode::FullPage => (
                page.screenshot(
                    ScreenshotParams::builder()
                        .format(CaptureScreenshotFormat::Png)
                        .full_page(true)
                        .build(),
                )
                .await
                .map_err(screenshot_error)?,
                None,
            ),
            ScreenshotMode::Element { target } => {
                let resolved = self.resolve_target(page_id, page, "", Some(target)).await?;
                let bytes = resolved.screenshot(page).await.map_err(|error| {
                    driver_error(ErrorCode::ScreenshotCaptureFailed, error.message)
                })?;
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
                    )
                    .await
                    .map_err(screenshot_error)?,
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
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
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
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
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
    // evaluation — the token capability check (`AuthenticatedRuntime::submit`)
    // and the per-session `ExecutionPolicy` check (`RuntimeService::submit`) —
    // are enforced upstream of this worker; both land before `execute()` ever
    // reaches a `BrowserWorker`. A worker-level backstop was considered here
    // for defense-in-depth, but ChromiumWorker is launched via
    // `WorkerFactory::launch(&SessionId)` — it has no access to the session's
    // ExecutionPolicy, and neither does anything upstream of it in this crate
    // (page-runtime's `WorkerPool::lease` only threads a `SessionId`, not
    // session state; session-manager, which owns `ExecutionPolicy`, isn't
    // even a dependency of worker-pool or page-runtime today). Threading it
    // in would mean changing the `WorkerFactory`/`WorkerPool::lease`
    // signatures and pulling session state into worker-pool — real
    // architectural surface. The worker-level backstop remains deliberately
    // absent per that design tradeoff; the two gates above are relied on as
    // authoritative.
    async fn evaluate_javascript(
        &self,
        page_id: &PageId,
        command: &EvaluateJavaScriptCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;

        let mut params = EvaluateParams::new(command.expression.clone());
        params.await_promise = Some(command.await_promise);
        params.return_by_value = Some(true);

        // DoS clamp: a caller-supplied `timeout_ms` is otherwise unbounded, which would
        // let a valid (capability- and policy-cleared) caller pin a worker lease open
        // arbitrarily long. Clamp to the configured ceiling rather than rejecting — the
        // command still runs, just under the same bound every other evaluation is held to.
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
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
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
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
        let current_url =
            page.url().await.map_err(command_failed)?.ok_or_else(|| {
                driver_error(ErrorCode::InvalidRequest, "page URL is unavailable")
            })?;
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

/// DoS clamp for `EvaluateJavaScript::timeout_ms`: bounds a caller-requested timeout to the
/// configured `max_js_timeout_ms` ceiling so a valid (capability- and policy-cleared) caller
/// cannot pin a worker lease open indefinitely by requesting an arbitrarily large timeout.
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

    // The underlying reap/register/kill mechanics are engine-agnostic and
    // tested directly in `process_registry`. These tests cover only the
    // Chrome-specific layer on top: the PID-registry key derivation
    // (`WorkerId`-keyed) and the "chrom" identity check.

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
