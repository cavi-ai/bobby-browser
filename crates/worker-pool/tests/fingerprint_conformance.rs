//! Live Chromium fingerprint conformance (ignored without Chrome).

use fingerprinting::{
    build_collector_probe_script, build_font_probe_script, build_probe_script,
    build_worker_probe_script, FingerprintConfig,
};
use std::path::PathBuf;
use types::{
    EvaluateJavaScriptCommand, Evidence, NavigateCommand, OpenPageCommand, SessionId, WaitUntil,
};
use worker_pool::{BrowserWorker, ChromiumWorkerFactory, WorkerFactory};

fn chrome_headed() -> bool {
    matches!(
        std::env::var("BOBBY_FP_HEADED").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn chrome_config(root: &std::path::Path) -> config::BrowserConfig {
    config::BrowserConfig {
        // `BOBBY_CHROME_EXECUTABLE` first: it is what CI sets and what every other live
        // suite reads. `CHROME_PATH` stays as a fallback for existing local setups.
        executable: std::env::var_os("BOBBY_CHROME_EXECUTABLE")
            .or_else(|| std::env::var_os("CHROME_PATH"))
            .map(PathBuf::from)
            .or_else(|| {
                Some(PathBuf::from(
                    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                ))
            }),
        profiles_dir: root.join("profiles"),
        headless: !chrome_headed(),
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

async fn open_and_navigate(
    worker: &dyn BrowserWorker,
    url: &str,
) -> (types::PageId, serde_json::Value) {
    let pages = worker_pool::tabs_or_default(worker.tabs())
        .open_page_command(&OpenPageCommand {
            url: Some("about:blank".into()),
        })
        .await
        .unwrap();
    let page_id = match &pages[0] {
        Evidence::Page { page_id, .. } => page_id.clone(),
        other => panic!("expected page evidence, got {other:?}"),
    };

    worker_pool::navigation_or_default(worker.navigation())
        .navigate(
            &page_id,
            &NavigateCommand {
                url: url.into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 15_000,
            },
        )
        .await
        .unwrap();

    let result = worker_pool::javascript_or_default(worker.javascript())
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
    (page_id, probe)
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
    assert!(
        worker_pool::session_settings_or_default(worker.session_settings()).fingerprint_enabled()
    );

    let (_page_id, probe) = open_and_navigate(worker.as_ref(), "https://example.com/").await;
    let session = fingerprinting::create_session(&fingerprint);

    assert_eq!(probe["fingerprintApplied"], true);
    assert_eq!(probe["userAgent"], session.user_agent);
    assert_eq!(probe["platform"], session.platform);
    assert_eq!(probe["screen"]["width"], session.screen_resolution.width);
    assert_eq!(probe["webglVendor"], session.webgl.vendor);
    assert_eq!(probe["webglRenderer"], session.webgl.renderer);
    assert!(probe["webdriver"].is_null() || probe["webdriver"] == false);
    assert_eq!(probe["canvasHashStable"], true);
    assert_eq!(probe["hasBobbyMarker"], false);
    if let Some(ua_data) = probe.get("userAgentData") {
        assert_eq!(ua_data["platform"], session.client_hints.platform);
        assert_eq!(ua_data["mobile"], false);
    }
    if let Some(plugins) = probe.get("pluginCount") {
        assert!(plugins.as_u64().unwrap_or(0) >= 1);
    }
    if let Some(rtc) = probe.get("rtcConstructible") {
        assert_eq!(rtc, true);
    }
    if let Some(count) = probe.get("mediaDeviceCount") {
        assert!(count.is_number(), "mediaDeviceCount should be numeric");
    }
    if let Some(max_tex) = probe.get("webglMaxTextureSize") {
        assert_eq!(max_tex, session.webgl.max_texture_size);
    }
    if let Some(effective) = probe.get("connectionEffectiveType") {
        assert_eq!(effective, "4g");
    }
    if let Some(level) = probe.get("batteryLevel") {
        assert_eq!(level, 1.0);
    }

    worker_pool::session_settings_or_default(worker.session_settings())
        .set_fingerprint_enabled(false)
        .await
        .unwrap();
    assert!(
        !worker_pool::session_settings_or_default(worker.session_settings()).fingerprint_enabled()
    );
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn chromium_worker_ua_matches_session() {
    let root = tempfile::tempdir().unwrap();
    let fingerprint = FingerprintConfig::default()
        .with_enabled(true)
        .with_session_seed(54321);
    let factory = ChromiumWorkerFactory::new(chrome_config(root.path()))
        .with_fingerprint(fingerprint.clone());
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let session = fingerprinting::create_session(&fingerprint);

    let page_id = open_page(worker.as_ref()).await;
    navigate(worker.as_ref(), &page_id, "https://example.com/").await;

    let probe = eval_json(
        worker.as_ref(),
        &page_id,
        &build_worker_probe_script(),
        15_000,
    )
    .await;

    eprintln!(
        "worker probe: {}",
        serde_json::to_string_pretty(&probe).unwrap()
    );

    let worker_ua = probe["worker"]["ua"].as_str().expect("worker ua");
    let worker_platform = probe["worker"]["platform"]
        .as_str()
        .expect("worker platform");
    assert_eq!(worker_ua, session.user_agent);
    assert_eq!(worker_platform, session.platform);
    assert!(
        probe["worker"]["webdriver"].is_null() || probe["worker"]["webdriver"] == false,
        "worker webdriver should be false/null"
    );
    assert!(
        !worker_ua.contains("HeadlessChrome"),
        "worker UA leaked headless: {worker_ua}"
    );
    assert_eq!(
        probe["page"]["uaDataPlatform"], session.client_hints.platform,
        "page userAgentData.platform must match session"
    );
    if let Some(he) = probe["page"].get("highEntropy") {
        if let Some(full) = he.get("uaFullVersion").and_then(|v| v.as_str()) {
            assert_eq!(
                full, session.client_hints.full_version,
                "page uaFullVersion must match session, got {full}"
            );
        }
    }
    assert_eq!(
        probe["worker"]["uaDataPlatform"], session.client_hints.platform,
        "worker userAgentData.platform must match session"
    );
    if let Some(he) = probe["worker"].get("highEntropy") {
        assert_eq!(
            he["platform"], session.client_hints.platform,
            "worker high-entropy platform must match session"
        );
        if let Some(full) = he.get("uaFullVersion").and_then(|v| v.as_str()) {
            assert_eq!(
                full, session.client_hints.full_version,
                "worker uaFullVersion must match session, got {full}"
            );
        }
    }

    if let Some(shared) = probe.get("shared").and_then(|v| v.as_object()) {
        let shared_ua = shared["ua"].as_str().expect("shared ua");
        let shared_platform = shared["platform"].as_str().expect("shared platform");
        assert_eq!(shared_ua, session.user_agent);
        assert_eq!(shared_platform, session.platform);
        assert!(
            shared
                .get("webdriver")
                .map(|v| v.is_null() || *v == false)
                .unwrap_or(true),
            "shared worker webdriver should be false/null"
        );
        assert!(
            !shared_ua.contains("HeadlessChrome"),
            "shared worker UA leaked headless: {shared_ua}"
        );
        assert_eq!(
            shared.get("uaDataPlatform"),
            Some(&serde_json::Value::String(
                session.client_hints.platform.clone()
            )),
            "shared userAgentData.platform must match session"
        );
        if let Some(he) = shared.get("highEntropy") {
            if let Some(full) = he.get("uaFullVersion").and_then(|v| v.as_str()) {
                assert_eq!(
                    full, session.client_hints.full_version,
                    "shared uaFullVersion must match session, got {full}"
                );
            }
        }
        assert_eq!(
            shared.get("bootstrapApplied"),
            Some(&serde_json::Value::Bool(true)),
            "shared worker must run bobby.fp.worker bootstrap"
        );
    }
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn chromium_font_mask_hides_host_fonts() {
    let root = tempfile::tempdir().unwrap();
    let fingerprint = FingerprintConfig::default()
        .with_enabled(true)
        .with_session_seed(31415);
    let factory =
        ChromiumWorkerFactory::new(chrome_config(root.path())).with_fingerprint(fingerprint);
    let worker = factory.launch(&SessionId::new()).await.unwrap();

    let page_id = open_page(worker.as_ref()).await;
    navigate(worker.as_ref(), &page_id, "https://example.com/").await;

    let probe = eval_json(
        worker.as_ref(),
        &page_id,
        &build_font_probe_script(),
        15_000,
    )
    .await;

    eprintln!(
        "font probe: {}",
        serde_json::to_string_pretty(&probe).unwrap()
    );

    assert_eq!(probe["fingerprintApplied"], true);
    assert_eq!(
        probe["offset"]["helveticaHidden"], true,
        "Helvetica Neue must measure like monospace fallback"
    );
    assert_eq!(
        probe["offset"]["pingfangHidden"], true,
        "PingFang must measure like monospace fallback"
    );
    assert_eq!(
        probe["measureText"]["helveticaHidden"], true,
        "canvas measureText must hide Helvetica Neue"
    );
    assert_eq!(
        probe["fontsCheck"]["helvetica"], false,
        "document.fonts.check must deny Helvetica Neue"
    );
    assert_eq!(
        probe["fontFaceLoad"]["helvetica"], false,
        "FontFace.local(Helvetica Neue) must fail under Windows allowlist"
    );
    assert_eq!(probe["touch"]["maxTouchPoints"], 0);
    assert_eq!(
        probe["touch"]["hasTouch"], false,
        "hasTouch() must be false for desktop persona"
    );
    assert_eq!(
        probe["touch"]["anyPointerCoarse"], false,
        "any-pointer:coarse must be false for desktop persona"
    );
    assert_eq!(
        probe["touch"]["anyPointerFine"], true,
        "any-pointer:fine must be true for desktop persona"
    );
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn chromium_fingerprint_toggle_navigate_new_pages() {
    let root = tempfile::tempdir().unwrap();
    let fingerprint = FingerprintConfig::default()
        .with_enabled(true)
        .with_session_seed(4242);
    let factory = ChromiumWorkerFactory::new(chrome_config(root.path()))
        .with_fingerprint(fingerprint.clone());
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let session = fingerprinting::create_session(&fingerprint);

    let (_page_on, probe_on) = open_and_navigate(worker.as_ref(), "https://example.com/").await;
    assert_eq!(probe_on["fingerprintApplied"], true);
    assert_eq!(probe_on["userAgent"], session.user_agent);
    assert_eq!(probe_on["canvasHashStable"], true);

    worker_pool::session_settings_or_default(worker.session_settings())
        .set_fingerprint_enabled(false)
        .await
        .unwrap();
    let (_page_off, probe_off) = open_and_navigate(worker.as_ref(), "https://example.org/").await;
    assert_eq!(
        probe_off["fingerprintApplied"], false,
        "new page after disable must not carry bobby fingerprint marker"
    );
    assert_ne!(
        probe_off["userAgent"], session.user_agent,
        "disabled path should not force session UA"
    );

    worker_pool::session_settings_or_default(worker.session_settings())
        .set_fingerprint_enabled(true)
        .await
        .unwrap();
    let (_page_re, probe_re) = open_and_navigate(worker.as_ref(), "https://example.net/").await;
    assert_eq!(probe_re["fingerprintApplied"], true);
    assert_eq!(probe_re["userAgent"], session.user_agent);
    assert_eq!(probe_re["platform"], session.platform);
    assert_eq!(probe_re["canvasHashStable"], true);
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn chromium_collector_dogfood_passes() {
    let root = tempfile::tempdir().unwrap();
    let fingerprint = FingerprintConfig::default()
        .with_enabled(true)
        .with_session_seed(99999);
    let factory =
        ChromiumWorkerFactory::new(chrome_config(root.path())).with_fingerprint(fingerprint);
    let worker = factory.launch(&SessionId::new()).await.unwrap();

    let pages = worker_pool::tabs_or_default(worker.tabs())
        .open_page_command(&OpenPageCommand {
            url: Some("about:blank".into()),
        })
        .await
        .unwrap();
    let page_id = match &pages[0] {
        Evidence::Page { page_id, .. } => page_id.clone(),
        other => panic!("expected page evidence, got {other:?}"),
    };

    worker_pool::navigation_or_default(worker.navigation())
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

    let result = worker_pool::javascript_or_default(worker.javascript())
        .evaluate_javascript(
            &page_id,
            &EvaluateJavaScriptCommand {
                expression: build_collector_probe_script(),
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

    if probe["passed"] != true || probe["failCount"].as_u64().unwrap_or(1) != 0 {
        eprintln!("collector probe fails: {}", probe["fails"]);
    }
    assert_eq!(
        probe["passed"], true,
        "collector probe failed: {}",
        probe["fails"]
    );
    assert_eq!(probe["failCount"], 0);
}

async fn eval_json(
    worker: &dyn BrowserWorker,
    page_id: &types::PageId,
    expression: &str,
    timeout_ms: u64,
) -> serde_json::Value {
    eval_json_ex(worker, page_id, expression, timeout_ms, true).await
}

async fn eval_json_ex(
    worker: &dyn BrowserWorker,
    page_id: &types::PageId,
    expression: &str,
    timeout_ms: u64,
    await_promise: bool,
) -> serde_json::Value {
    let result = worker_pool::javascript_or_default(worker.javascript())
        .evaluate_javascript(
            page_id,
            &EvaluateJavaScriptCommand {
                expression: expression.into(),
                timeout_ms,
                await_promise,
            },
        )
        .await
        .unwrap_or_else(|e| panic!("eval_json failed: {e:?}"));

    match result.as_slice() {
        [Evidence::JavaScriptResult { value, .. }] => value.clone(),
        other => panic!("expected javascript result, got {other:?}"),
    }
}

async fn navigate(worker: &dyn BrowserWorker, page_id: &types::PageId, url: &str) {
    worker_pool::navigation_or_default(worker.navigation())
        .navigate(
            page_id,
            &NavigateCommand {
                url: url.into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 30_000,
            },
        )
        .await
        .unwrap();
}

async fn open_page(worker: &dyn BrowserWorker) -> types::PageId {
    let pages = worker_pool::tabs_or_default(worker.tabs())
        .open_page_command(&OpenPageCommand {
            url: Some("about:blank".into()),
        })
        .await
        .unwrap();
    match &pages[0] {
        Evidence::Page { page_id, .. } => page_id.clone(),
        other => panic!("expected page evidence, got {other:?}"),
    }
}
