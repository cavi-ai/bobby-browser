use async_trait::async_trait;
use std::sync::Mutex;
use types::{CommandError, ErrorCode, ErrorLayer, EvaluateJavaScriptCommand, Evidence, PageId};
use worker_pool::{CaptureEngine, JavaScriptEngine, PageBehavior};

#[derive(Default)]
struct Capture {
    calls: Mutex<Vec<String>>,
    install_error: bool,
    capture_error: bool,
    cleanup_error: bool,
}
fn error(message: &str) -> CommandError {
    CommandError {
        code: ErrorCode::BrowserCommandFailed,
        layer: ErrorLayer::Driver,
        retryable: false,
        message: message.into(),
    }
}
#[async_trait]
impl CaptureEngine for Capture {
    async fn screenshot_bytes(&self, _: &PageId) -> Result<Vec<u8>, CommandError> {
        self.calls.lock().unwrap().push("capture".into());
        if self.capture_error {
            Err(error("capture"))
        } else {
            Ok(vec![1, 2, 3])
        }
    }
}
#[async_trait]
impl JavaScriptEngine for Capture {
    async fn evaluate_javascript(
        &self,
        _: &PageId,
        command: &EvaluateJavaScriptCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        assert_eq!(command.timeout_ms, 5_000);
        assert!(!command.await_promise);
        let install = command.expression.contains("createElement");
        self.calls.lock().unwrap().push(command.expression.clone());
        if install && self.install_error {
            Err(error("install"))
        } else if !install && self.cleanup_error {
            Err(error("cleanup"))
        } else {
            Ok(vec![])
        }
    }
}
async fn run(capture: &Capture) -> Result<Vec<u8>, CommandError> {
    PageBehavior::sanitized_screenshot_bytes(Some(capture), Some(capture), &PageId::new()).await
}
#[tokio::test]
async fn mask_install_failure_attempts_cleanup() {
    let capture = Capture {
        install_error: true,
        cleanup_error: true,
        ..Capture::default()
    };
    assert_eq!(run(&capture).await.unwrap_err().message, "install");
    let calls = capture.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|v| v != "capture"));
}
#[tokio::test]
async fn capture_failure_keeps_capture_error_even_if_cleanup_fails() {
    let capture = Capture {
        capture_error: true,
        cleanup_error: true,
        ..Capture::default()
    };
    assert_eq!(run(&capture).await.unwrap_err().message, "capture");
    let calls = capture.calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[1], "capture");
}
#[tokio::test]
async fn cleanup_failure_with_successful_capture_returns_error() {
    let capture = Capture {
        cleanup_error: true,
        ..Capture::default()
    };
    assert_eq!(run(&capture).await.unwrap_err().message, "cleanup");
}
#[tokio::test]
async fn successful_capture_returns_bytes_after_cleanup() {
    let capture = Capture::default();
    assert_eq!(run(&capture).await.unwrap(), [1, 2, 3]);
    assert_eq!(run(&capture).await.unwrap(), [1, 2, 3]);
    let calls = capture.calls.lock().unwrap();
    assert_eq!(calls.len(), 6);
    assert_eq!(calls[1], "capture");
    assert_eq!(calls[4], "capture");
    assert_ne!(calls[0], calls[3], "each mask has its own identity");
    // Each cleanup must remove the token installed by that same capture.
    for offset in [0, 3] {
        let install = &calls[offset];
        let start = install.find("bobby-corpus-mask-").unwrap();
        let token = install[start..].split('"').next().unwrap();
        assert!(calls[offset + 2].contains(token));
    }
}
#[tokio::test]
async fn missing_javascript_cannot_produce_a_successful_capture() {
    let capture = Capture::default();
    assert!(
        PageBehavior::sanitized_screenshot_bytes(Some(&capture), None, &PageId::new())
            .await
            .is_err()
    );
    assert!(capture.calls.lock().unwrap().is_empty());
}
