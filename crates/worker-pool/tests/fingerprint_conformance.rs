//! Live Chromium fingerprint conformance (ignored without Chrome).

use config::BrowserConfig;
use fingerprinting::{build_probe_script, FingerprintConfig};
use std::path::PathBuf;
use types::{
    EvaluateJavaScriptCommand, Evidence, NavigateCommand, OpenPageCommand, SessionId, WaitUntil,
};
use worker_pool::{ChromiumWorkerFactory, WorkerFactory};

fn chrome_config(root: &std::path::Path) -> BrowserConfig {
    BrowserConfig {
        executable: std::env::var_os("CHROME_PATH")
            .map(PathBuf::from)
            .or_else(|| {
                Some(PathBuf::from(
                    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                ))
            }),
        profiles_dir: root.join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.to_path_buf()],
        downloads_dir: root.join("downloads"),
        artifacts_dir: root.join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 256 * 1024,
        max_js_timeout_ms: 30_000,
    }
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn chromium_fingerprint_probe_matches_session() {
    let root = tempfile::tempdir().unwrap();
    let fingerprint = FingerprintConfig::default()
        .with_enabled(true)
        .with_session_seed(12345);
    let factory = ChromiumWorkerFactory::new(chrome_config(root.path()))
        .with_fingerprint(fingerprint.clone());
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    assert!(worker.fingerprint_enabled());

    let pages = worker
        .open_page_command(&OpenPageCommand {
            url: Some("about:blank".into()),
        })
        .await
        .unwrap();
    let page_id = match &pages[0] {
        Evidence::Page { page_id, .. } => page_id.clone(),
        other => panic!("expected page evidence, got {other:?}"),
    };

    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: "https://example.com/".into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 15_000,
            },
        )
        .await
        .unwrap();

    let result = worker
        .evaluate_javascript(
            &page_id,
            &EvaluateJavaScriptCommand {
                expression: build_probe_script(),
                timeout_ms: 10_000,
                await_promise: true,
            },
        )
        .await
        .unwrap();

    let probe = match result.as_slice() {
        [Evidence::JavaScriptResult { value, .. }] => value.clone(),
        other => panic!("expected javascript result, got {other:?}"),
    };
    let session = fingerprinting::create_session(&fingerprint);

    assert_eq!(probe["fingerprintApplied"], true);
    assert_eq!(probe["userAgent"], session.user_agent);
    assert_eq!(probe["platform"], session.platform);
    assert_eq!(probe["screen"]["width"], session.screen_resolution.width);
    assert_eq!(probe["webglVendor"], session.webgl.vendor);
    assert_eq!(probe["webglRenderer"], session.webgl.renderer);
    assert!(probe["webdriver"].is_null() || probe["webdriver"] == false);

    worker.set_fingerprint_enabled(false);
    assert!(!worker.fingerprint_enabled());
}
