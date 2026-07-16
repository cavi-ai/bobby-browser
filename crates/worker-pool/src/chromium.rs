use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chromiumoxide::browser::{Browser, BrowserConfig as ChromiumConfig};
use chromiumoxide::Page;
use config::BrowserConfig;
use futures::StreamExt;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use types::{
    ClickCommand, CommandError, ErrorCode, ErrorLayer, Evidence, InspectCommand, NavigateCommand,
    PageId, SessionId, TypeTextCommand, WorkerId,
};

use crate::{session_download_dir, BrowserWorker, WorkerFactory};

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
            _upload_roots: self.config.upload_roots.clone(),
            _download_dir: download_dir,
            browser: Mutex::new(Some(browser)),
            pages: Mutex::new(HashMap::new()),
            handler_task: Mutex::new(Some(handler_task)),
        }))
    }
}

struct ChromiumWorker {
    id: WorkerId,
    profile_dir: PathBuf,
    _upload_roots: Vec<PathBuf>,
    _download_dir: PathBuf,
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
