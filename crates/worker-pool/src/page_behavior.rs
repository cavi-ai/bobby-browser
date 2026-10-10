//! Existing shared page behavior over narrow engine views.
use crate::{
    corpus_mask_cleanup_script, corpus_mask_install_script, require_domain, CaptureEngine,
    JavaScriptEngine, WaitProvider,
};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use types::{CommandError, EvaluateJavaScriptCommand, Evidence, PageId, WaitForCommand};
pub struct PageBehavior;
impl PageBehavior {
    pub async fn wait_for(
        provider: Option<&dyn WaitProvider>,
        page_id: &PageId,
        command: &WaitForCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let observer = require_domain(provider)?.observer(page_id);
        crate::wait::poll_until(command, observer.as_ref()).await
    }
    pub async fn sanitized_screenshot_bytes(
        capture: Option<&dyn CaptureEngine>,
        javascript: Option<&dyn JavaScriptEngine>,
        page_id: &PageId,
    ) -> Result<Vec<u8>, CommandError> {
        let javascript = require_domain(javascript)?;
        static MASK_SEQUENCE: AtomicU64 = AtomicU64::new(1);
        let token = format!(
            "bobby-corpus-mask-{}",
            MASK_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed)
        );
        let install = corpus_mask_install_script(&token);
        let install_result = javascript
            .evaluate_javascript(
                page_id,
                &EvaluateJavaScriptCommand {
                    expression: install,
                    timeout_ms: 5_000,
                    await_promise: false,
                },
            )
            .await;
        if let Err(error) = install_result {
            let _ = javascript
                .evaluate_javascript(
                    page_id,
                    &EvaluateJavaScriptCommand {
                        expression: corpus_mask_cleanup_script(&token),
                        timeout_ms: 5_000,
                        await_promise: false,
                    },
                )
                .await;
            return Err(error);
        }
        let capture = match require_domain(capture) {
            Ok(capture) => capture.screenshot_bytes(page_id).await,
            Err(error) => Err(error),
        };
        let cleanup = javascript
            .evaluate_javascript(
                page_id,
                &EvaluateJavaScriptCommand {
                    expression: corpus_mask_cleanup_script(&token),
                    timeout_ms: 5_000,
                    await_promise: false,
                },
            )
            .await;
        match (capture, cleanup) {
            (Ok(bytes), Ok(_)) => Ok(bytes),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }
}
