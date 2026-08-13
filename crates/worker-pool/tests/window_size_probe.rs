use fingerprinting::FingerprintConfig;
use types::{Evidence, PageId, SessionId};
use worker_pool::{ChromiumWorkerFactory, WorkerFactory};

fn chrome_config(root: &std::path::Path) -> config::BrowserConfig {
    config::BrowserConfig {
        executable: Some(std::path::PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        )),
        profiles_dir: root.join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![],
        downloads_dir: root.join("downloads"),
        artifacts_dir: root.join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    }
}

/// Regression for the CreepJS screen/media-query leak: the browser's native
/// metrics (screen.width, device-width media queries, inner/outer window
/// bounds) must agree with the spoofed fingerprint profile. A default 800x600
/// headless window against a 1920x1080 spoofed screen was flagged as
/// "like headless"; the profile must also keep innerWidth != screen.width
/// (hasVvpScreenRes), pdfViewerEnabled true, and navigator.share present.
#[tokio::test]
#[ignore = "requires Chrome; no network needed"]
async fn fingerprint_screen_metrics_are_consistent_across_channels() {
    let root = tempfile::tempdir().unwrap();
    let fingerprint = FingerprintConfig::default()
        .with_enabled(true)
        .with_session_seed(777)
        .with_inject_chrome(true);
    let factory =
        ChromiumWorkerFactory::new(chrome_config(root.path())).with_fingerprint(fingerprint);
    let worker = factory.launch(&SessionId::new()).await.unwrap();

    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();
    // The fingerprint init script is a preload: it runs on the next document
    // load, so navigate after opening for it to take effect.
    worker
        .navigate(
            &page,
            &types::NavigateCommand {
                url: "data:text/html,<title>probe</title>".into(),
                wait_until: types::WaitUntil::Commit,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();
    let outcome = worker
        .evaluate_javascript(
            &page,
            &types::EvaluateJavaScriptCommand {
                expression: r#"({
                    innerW: window.innerWidth,
                    innerH: window.innerHeight,
                    outerW: window.outerWidth,
                    outerH: window.outerHeight,
                    screenW: screen.width,
                    screenH: screen.height,
                    availW: screen.availWidth,
                    availH: screen.availHeight,
                    mq800: matchMedia('(device-width: 800px)').matches,
                    mq1920: matchMedia('(device-width: 1920px)').matches,
                    dpr: window.devicePixelRatio,
                    pdfViewer: navigator.pdfViewerEnabled,
                    hasShare: 'share' in navigator && 'canShare' in navigator,
                    vvpScreenRes: (window.innerWidth === screen.width && window.outerHeight === screen.height),
                })"#
                .into(),
                await_promise: false,
                timeout_ms: 5_000,
            },
        )
        .await
        .unwrap();

    let metrics = outcome
        .iter()
        .find_map(|item| match item {
            Evidence::JavaScriptResult { value, .. } => Some(value.clone()),
            _ => None,
        })
        .expect("javascript metrics evidence");

    assert_eq!(metrics["screenW"], 1920, "screen.width");
    assert_eq!(metrics["screenH"], 1080, "screen.height");
    assert_eq!(
        metrics["availH"], 1040,
        "avail height keeps the taskbar inset"
    );
    assert_eq!(
        metrics["mq800"], false,
        "device-width media query must not see the headless default"
    );
    assert_eq!(
        metrics["mq1920"], true,
        "device-width media query must match the spoofed screen"
    );
    assert_eq!(
        metrics["vvpScreenRes"], false,
        "window must not fill the screen exactly"
    );
    assert_ne!(
        metrics["innerW"], metrics["screenW"],
        "innerWidth must differ from screen.width"
    );
    assert!(
        metrics["innerW"].as_u64().unwrap() < metrics["availW"].as_u64().unwrap(),
        "window fits inside the available area"
    );
    assert!(
        metrics["outerH"].as_u64().unwrap() < metrics["screenH"].as_u64().unwrap(),
        "outer height stays below the full screen"
    );
    assert_eq!(
        metrics["pdfViewer"], true,
        "pdfViewerEnabled matches desktop Chrome"
    );
    assert_eq!(
        metrics["hasShare"], true,
        "Web Share API present as on desktop Chrome"
    );
}
