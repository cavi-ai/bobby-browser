use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use artifact_store::ArtifactStore;
use async_trait::async_trait;
use chromiumoxide::browser::{Browser, BrowserConfig as ChromiumConfig};
use chromiumoxide::cdp::browser_protocol::browser::{
    DownloadProgressState, EventDownloadProgress, EventDownloadWillBegin,
    SetDownloadBehaviorBehavior, SetDownloadBehaviorParams,
};
use chromiumoxide::cdp::browser_protocol::page::{CaptureScreenshotFormat, Viewport};
use chromiumoxide::cdp::browser_protocol::target::EventTargetCreated;
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::Page;
use config::BrowserConfig;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use types::{
    CaptureScreenshotCommand, ClickAndWaitForDownloadCommand, ClickAndWaitForPopupCommand,
    ClickCommand, ClosePageCommand, CommandError, ErrorCode, ErrorLayer, Evidence, InspectCommand,
    ListPagesCommand, NavigateCommand, OpenPageCommand, PageEvidence, PageId, ScreenshotMode,
    SessionId, TypeTextCommand, UploadFilesCommand, WaitCondition, WaitForCommand, WaitUntil,
    WorkerId,
};

use crate::{
    resolve_upload_paths, session_download_dir,
    targeting::{resolve_target as resolve_browser_target, resolve_target_with_visibility},
    BrowserWorker, WorkerFactory,
};

#[derive(Clone)]
pub struct ChromiumWorkerFactory {
    config: BrowserConfig,
}

impl ChromiumWorkerFactory {
    pub fn new(config: BrowserConfig) -> Self {
        Self { config }
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
        let (browser, mut handler) = Browser::launch(config)
            .await
            .map_err(|error| driver_error(ErrorCode::BrowserLaunchFailed, error))?;
        let handler_task = tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                if event.is_err() {
                    break;
                }
            }
        });

        Ok(Arc::new(ChromiumWorker {
            id: WorkerId::new(),
            profile_dir,
            upload_roots: self.config.upload_roots.clone(),
            download_dir,
            session_id: session_id.clone(),
            artifacts: ArtifactStore::new(
                self.config.artifacts_dir.clone(),
                self.config.max_artifact_bytes,
                self.config.max_screenshot_dimension,
            ),
            browser: Mutex::new(Some(browser)),
            pages: Mutex::new(HashMap::new()),
            handler_task: Mutex::new(Some(handler_task)),
        }))
    }
}

struct ChromiumWorker {
    id: WorkerId,
    profile_dir: PathBuf,
    upload_roots: Vec<PathBuf>,
    download_dir: PathBuf,
    session_id: SessionId,
    artifacts: ArtifactStore,
    browser: Mutex<Option<Browser>>,
    pages: Mutex<HashMap<PageId, Page>>,
    handler_task: Mutex<Option<JoinHandle<()>>>,
}

impl ChromiumWorker {
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
        self.pages.lock().await.insert(page_id, page);
        Ok(())
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

    async fn type_text(
        &self,
        page_id: &PageId,
        command: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
        let resolved = self
            .resolve_target(page_id, page, &command.selector, command.target.as_ref())
            .await?;
        resolved
            .type_text(page, &command.value, command.clear_first)
            .await?;
        Ok(vec![
            Evidence::Element {
                selector: command.selector.clone(),
                text: Some(command.value.clone()),
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
        self.pages.lock().await.insert(page_id, page);
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
            .pages
            .lock()
            .await
            .remove(&command.page_id)
            .ok_or_else(page_missing)?;
        let evidence = page_evidence(command.page_id.clone(), &page).await?;
        page.close().await.map_err(command_failed)?;
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
        self.pages.lock().await.insert(popup_id.clone(), popup);
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
            let pages = self.pages.lock().await;
            let page = pages.get(page_id).ok_or_else(page_missing)?;
            let satisfied = wait_condition_satisfied(
                &self.browser,
                page_id,
                page,
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

    async fn close(&self) -> Result<(), CommandError> {
        self.pages.lock().await.clear();
        if let Some(mut browser) = self.browser.lock().await.take() {
            browser.close().await.map_err(command_failed)?;
        }
        if let Some(task) = self.handler_task.lock().await.take() {
            task.abort();
        }
        Ok(())
    }

    async fn terminate(&self) -> Result<(), CommandError> {
        self.pages.lock().await.clear();
        let close_result = if let Some(mut browser) = self.browser.lock().await.take() {
            browser.close().await.map(|_| ()).map_err(command_failed)
        } else {
            Ok(())
        };
        if let Some(task) = self.handler_task.lock().await.take() {
            task.abort();
        }
        close_result
    }
}

async fn wait_condition_satisfied(
    browser: &Mutex<Option<Browser>>,
    page_id: &PageId,
    page: &Page,
    condition: &WaitCondition,
    quiet_since: &mut Option<Instant>,
) -> Result<bool, CommandError> {
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
                return Ok(matches!(state, types::ElementState::Detached));
            };
            let visible = resolved.visible(page).await?;
            let enabled = resolved.enabled(page).await?;
            Ok(match state {
                types::ElementState::Attached => true,
                types::ElementState::Detached => false,
                types::ElementState::Visible => visible,
                types::ElementState::Hidden => !visible,
                types::ElementState::Enabled => enabled,
                types::ElementState::Disabled => !enabled,
            })
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
                Err(error) if matches!(error.code, ErrorCode::TargetNotFound) => return Ok(false),
                Err(error) => return Err(error),
            };
            let value = if matches!(condition, WaitCondition::Value { .. }) {
                resolved.value(page).await?.unwrap_or_default()
            } else {
                resolved.inner_text(page).await?.unwrap_or_default()
            };
            text_matches(matcher, &value)
        }
        WaitCondition::Url { matcher } => {
            let url = page
                .url()
                .await
                .map_err(command_failed)?
                .unwrap_or_default();
            text_matches(matcher, &url)
        }
        WaitCondition::Document { ready } => {
            let state: String = page
                .evaluate("document.readyState")
                .await
                .map_err(command_failed)?
                .into_value()
                .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
            Ok(match ready {
                WaitUntil::Commit => true,
                WaitUntil::DomContentLoaded | WaitUntil::Interactive => {
                    state == "interactive" || state == "complete"
                }
                WaitUntil::NetworkIdle => state == "complete",
            })
        }
        WaitCondition::NetworkQuiet {
            idle_ms,
            max_in_flight,
        } => {
            let in_flight: usize = page
                .evaluate(
                    "performance.getEntriesByType('resource').filter(x => !x.responseEnd).length",
                )
                .await
                .map_err(command_failed)?
                .into_value()
                .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
            if in_flight <= *max_in_flight {
                let since = quiet_since.get_or_insert_with(Instant::now);
                Ok(since.elapsed() >= Duration::from_millis(*idle_ms))
            } else {
                *quiet_since = None;
                Ok(false)
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

fn timeout_error(timeout_ms: u64) -> CommandError {
    CommandError {
        code: ErrorCode::DeadlineExceeded,
        message: format!("browser command exceeded {timeout_ms}ms"),
        layer: ErrorLayer::Driver,
        retryable: true,
    }
}

#[cfg(test)]
mod tests {
    use super::text_matches;
    use types::{ErrorCode, TextMatch};

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
}
