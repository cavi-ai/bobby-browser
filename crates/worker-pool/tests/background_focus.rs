//! A screenshot never activates a visible page's target: on a headed browser,
//! `Target.activateTarget` raises the window and takes the desktop's focus.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use config::BrowserConfig;
use types::{CaptureScreenshotCommand, PageId, ScreenshotMode, SessionId};
use worker_pool::{ChromiumWorkerFactory, WorkerFactory};

fn chrome_executable() -> PathBuf {
    std::env::var("BOBBY_CHROME_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        })
}

#[derive(Clone, Default)]
struct Sent(Arc<Mutex<Vec<u8>>>);

impl Write for Sent {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn a_screenshot_of_a_visible_page_sends_no_target_activation() {
    let sent = Sent::default();
    let writer = sent.clone();
    tracing_subscriber::fmt()
        .with_env_filter("chromiumoxide::conn=trace")
        .with_writer(move || writer.clone())
        .init();

    let profiles = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: profiles.path().to_path_buf(),
        headless: true,
        max_active: 1,
        upload_roots: vec![profiles.path().to_path_buf()],
        downloads_dir: profiles.path().join("downloads"),
        artifacts_dir: profiles.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let page_id = PageId::new();
    worker_pool::tabs_or_default(worker.tabs())
        .open_page(page_id.clone())
        .await
        .unwrap();
    sent.0.lock().unwrap().clear();

    worker_pool::capture_or_default(worker.capture())
        .capture_screenshot(
            &page_id,
            &CaptureScreenshotCommand {
                mode: ScreenshotMode::Viewport,
            },
        )
        .await
        .unwrap();

    let requests = String::from_utf8(sent.0.lock().unwrap().clone()).unwrap();
    worker.close().await.unwrap();
    assert!(
        requests.contains("Page.captureScreenshot"),
        "the capture request was not recorded: {requests}"
    );
    assert!(
        !requests.contains("Target.activateTarget"),
        "capturing a visible page activated its target"
    );
}
