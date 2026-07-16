use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chromiumoxide::browser::{Browser, BrowserConfig as ChromiumConfig};
use chromiumoxide::cdp::browser_protocol::browser::{
    DownloadProgressState, EventDownloadProgress, EventDownloadWillBegin,
    SetDownloadBehaviorBehavior, SetDownloadBehaviorParams,
};
use chromiumoxide::cdp::browser_protocol::dom::SetFileInputFilesParams;
use chromiumoxide::cdp::browser_protocol::target::EventTargetCreated;
use chromiumoxide::Page;
use config::BrowserConfig;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use types::{
    ClickAndWaitForDownloadCommand, ClickAndWaitForPopupCommand, ClickCommand, ClosePageCommand,
    CommandError, ErrorCode, ErrorLayer, Evidence, InspectCommand, ListPagesCommand,
    NavigateCommand, OpenPageCommand, PageEvidence, PageId, SessionId, TypeTextCommand,
    UploadFilesCommand, WorkerId,
};

use crate::{resolve_upload_paths, session_download_dir, BrowserWorker, WorkerFactory};

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
    browser: Mutex<Option<Browser>>,
    pages: Mutex<HashMap<PageId, Page>>,
    handler_task: Mutex<Option<JoinHandle<()>>>,
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
        let browser = self.browser.lock().await;
        let browser = browser.as_ref().ok_or_else(closed_error)?;
        let page = browser
            .new_page("about:blank")
            .await
            .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
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
        let (text, html) = if let Some(selector) = &command.selector {
            let element = page.find_element(selector).await.map_err(command_failed)?;
            let text = match element.string_property("value").await {
                Ok(Some(value)) if !value.is_empty() => value,
                _ => element
                    .inner_text()
                    .await
                    .map_err(command_failed)?
                    .unwrap_or_default(),
            };
            let html = if command.include_html {
                Some(
                    element
                        .outer_html()
                        .await
                        .map_err(command_failed)?
                        .unwrap_or_default(),
                )
            } else {
                None
            };
            (text, html)
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
            (text, html)
        };
        Ok(vec![Evidence::Inspection {
            selector: command.selector.clone(),
            url,
            title,
            text,
            html,
        }])
    }

    async fn click(
        &self,
        page_id: &PageId,
        command: &ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
        let element = page
            .find_element(command.selector.as_str())
            .await
            .map_err(command_failed)?;
        let text = element.inner_text().await.ok().flatten();
        element.click().await.map_err(command_failed)?;
        Ok(vec![Evidence::Element {
            selector: command.selector.clone(),
            text,
        }])
    }

    async fn type_text(
        &self,
        page_id: &PageId,
        command: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let pages = self.pages.lock().await;
        let page = pages.get(page_id).ok_or_else(page_missing)?;
        let element = page
            .find_element(command.selector.as_str())
            .await
            .map_err(command_failed)?;
        element.click().await.map_err(command_failed)?;
        if command.clear_first {
            element
                .call_js_fn(
                    "function() { this.value = ''; this.dispatchEvent(new Event('input', { bubbles: true })); }",
                    false,
                )
                .await
                .map_err(command_failed)?;
        }
        element
            .type_str(command.value.as_str())
            .await
            .map_err(command_failed)?;
        Ok(vec![Evidence::Element {
            selector: command.selector.clone(),
            text: Some(command.value.clone()),
        }])
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
        let element = page
            .find_element(&command.selector)
            .await
            .map_err(command_failed)?;
        let path_strings = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        page.execute(
            SetFileInputFilesParams::builder()
                .files(path_strings.clone())
                .backend_node_id(element.backend_node_id)
                .build()
                .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?,
        )
        .await
        .map_err(command_failed)?;
        Ok(vec![Evidence::Upload {
            selector: command.selector.clone(),
            paths: path_strings,
        }])
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
        listed.sort_by(|left, right| left.page_id.0.cmp(&right.page_id.0));
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
        let browser = self.browser.lock().await;
        let browser = browser.as_ref().ok_or_else(closed_error)?;
        let mut events = browser
            .event_listener::<EventTargetCreated>()
            .await
            .map_err(command_failed)?;
        let pages = self.pages.lock().await;
        let opener = pages.get(page_id).ok_or_else(page_missing)?;
        let opener_target = opener.target_id().clone();
        opener
            .find_element(&command.selector)
            .await
            .map_err(command_failed)?
            .click()
            .await
            .map_err(command_failed)?;
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
        drop(pages);
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
        self.pages.lock().await.insert(popup_id.clone(), popup);
        Ok(vec![Evidence::Popup {
            opener_page_id: page_id.clone(),
            page_id: popup_id,
            url: page.url,
            title: page.title,
        }])
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
        let element = page
            .find_element(&command.selector)
            .await
            .map_err(command_failed)?;
        element
            .call_js_fn("function() { this.click(); }", false)
            .await
            .map_err(command_failed)?;
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
        Ok(vec![Evidence::Download {
            filename: begin.suggested_filename.clone(),
            path: path.to_string_lossy().into_owned(),
            bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
        }])
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
