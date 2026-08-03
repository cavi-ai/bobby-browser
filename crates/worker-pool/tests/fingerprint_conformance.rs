//! Live Chromium fingerprint conformance (ignored without Chrome).

use fingerprinting::{
    build_collector_probe_script, build_font_probe_script, build_probe_script,
    build_worker_probe_script, FingerprintConfig,
};
use serde_json::json;
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
        // `BOBBY_CHROME_EXECUTABLE` first: it is what CI sets and what every
        // other live suite reads. This file read only `CHROME_PATH`, so it
        // launched nothing in CI and failed with a bare
        // `BrowserLaunchFailed: No such file or directory`. `CHROME_PATH` is
        // kept as a fallback so existing local setups keep working.
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
                url: url.into(),
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
    assert!(worker.fingerprint_enabled());

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

    worker.set_fingerprint_enabled(false).await.unwrap();
    assert!(!worker.fingerprint_enabled());
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
        probe["touch"]["creepHasTouch"], false,
        "CreepJS hasTouch() must be false for desktop persona"
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

    worker.set_fingerprint_enabled(false).await.unwrap();
    let (_page_off, probe_off) = open_and_navigate(worker.as_ref(), "https://example.org/").await;
    assert_eq!(
        probe_off["fingerprintApplied"], false,
        "new page after disable must not carry bobby fingerprint marker"
    );
    assert_ne!(
        probe_off["userAgent"], session.user_agent,
        "disabled path should not force session UA"
    );

    worker.set_fingerprint_enabled(true).await.unwrap();
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
    let result = worker
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
    worker
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
    let pages = worker
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

async fn wait_for_body_text(
    worker: &dyn BrowserWorker,
    page_id: &types::PageId,
    predicate_js: &str,
    timeout_ms: u64,
    poll_ms: u64,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        let ready = eval_json_ex(worker, page_id, predicate_js, 5_000, false).await;
        if ready.as_bool() == Some(true) {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!("timed out waiting for page content after {timeout_ms}ms");
        }
        tokio::time::sleep(std::time::Duration::from_millis(poll_ms)).await;
    }
}

fn collect_soft_findings(report: &serde_json::Value) -> Vec<String> {
    let mut findings = Vec::new();
    let site = report["site"].as_str().unwrap_or("unknown");

    let patterns = [
        "headless",
        "webdriver",
        "inconsistenc",
        "stealth",
        "bot detected",
        "automation",
        "headlesschrome",
    ];

    let texts: Vec<String> = report["bodySnippet"]
        .as_str()
        .into_iter()
        .chain(report["bodyText"].as_str())
        .map(str::to_string)
        .collect();

    for text in texts {
        let lower = text.to_lowercase();
        for pattern in patterns {
            if lower.contains(pattern) {
                findings.push(format!("{site}: body mentions '{pattern}'"));
            }
        }
    }

    if let Some(hints) = report["lieHints"].as_array() {
        for hint in hints {
            if let Some(s) = hint.as_str() {
                findings.push(format!("{site}: lie hint '{s}'"));
            }
        }
    }

    findings
}

#[tokio::test]
#[ignore = "requires Chrome + network; production collector dogfood"]
async fn chromium_production_collector_dogfood() {
    eprintln!("headless={}", !chrome_headed());
    let root = tempfile::tempdir().unwrap();
    let fingerprint = FingerprintConfig::default()
        .with_enabled(true)
        .with_session_seed(777)
        .with_inject_chrome(true);
    let factory =
        ChromiumWorkerFactory::new(chrome_config(root.path())).with_fingerprint(fingerprint);
    let worker = factory.launch(&SessionId::new()).await.unwrap();

    let mut reports: Vec<serde_json::Value> = Vec::new();
    let mut soft_findings: Vec<String> = Vec::new();

    // A. BrowserLeaks JS
    {
        let page_id = open_page(worker.as_ref()).await;
        navigate(
            worker.as_ref(),
            &page_id,
            "https://browserleaks.com/javascript",
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let report = eval_json(
            worker.as_ref(),
            &page_id,
            r#"({
  site: "browserleaks-js",
  userAgent: navigator.userAgent,
  platform: navigator.platform,
  webdriver: navigator.webdriver,
  vendor: navigator.vendor,
  languages: [...navigator.languages],
  hardwareConcurrency: navigator.hardwareConcurrency,
  deviceMemory: navigator.deviceMemory,
  plugins: navigator.plugins?.length,
  chrome: typeof chrome !== "undefined",
  chromeRuntime: !!(window.chrome && chrome.runtime),
  fingerprintApplied: !!globalThis[Symbol.for("bobby.fp.applied")],
  bodySnippet: document.body?.innerText?.slice(0, 2500) || ""
})"#,
            15_000,
        )
        .await;
        eprintln!(
            "=== BrowserLeaks JS ===\n{}",
            serde_json::to_string_pretty(&report).unwrap()
        );
        soft_findings.extend(collect_soft_findings(&report));
        reports.push(report);
    }

    // B. CreepJS
    {
        let page_id = open_page(worker.as_ref()).await;
        navigate(
            worker.as_ref(),
            &page_id,
            "https://abrahamjuliot.github.io/creepjs/",
        )
        .await;
        wait_for_body_text(
            worker.as_ref(),
            &page_id,
            r#"(function() {
  const t = document.body?.innerText || "";
  return t.includes("FP ID") && !t.includes("Computing...") && (t.includes("lies") || t.length > 800);
})()"#,
            30_000,
            1000,
        )
        .await;
        let mut report = eval_json(
            worker.as_ref(),
            &page_id,
            r#"(async () => {
  const text = document.body?.innerText || "";
  let hasBadChromeRuntime = false;
  try {
    if ('chrome' in window && chrome.runtime) {
      try {
        if ('prototype' in chrome.runtime.sendMessage || 'prototype' in chrome.runtime.connect) {
          hasBadChromeRuntime = true;
        } else {
          try { new chrome.runtime.sendMessage; hasBadChromeRuntime = true; } catch (err) {
            if (err?.constructor?.name !== 'TypeError') hasBadChromeRuntime = true;
          }
        }
      } catch (_) { hasBadChromeRuntime = true; }
    }
  } catch (_) {}
  const hasToStringProxy = (() => {
    try {
      return Function.prototype.toString.toString().indexOf('[native code]') < 0;
    } catch (_) { return true; }
  })();
  // Approximate CreepJS webDriverIsOn without lieProps: true webdriver OR
  // a non-native prototype getter (our old redefine).
  let webdriverGetterNative = true;
  try {
    const desc = Object.getOwnPropertyDescriptor(Navigator.prototype, 'webdriver');
    if (desc && desc.get) {
      webdriverGetterNative = Function.prototype.toString.call(desc.get).indexOf('[native code]') >= 0;
    }
  } catch (_) {}
  const webDriverIsOn = (
    (CSS.supports('border-end-end-radius: initial') && navigator.webdriver === undefined) ||
    !!navigator.webdriver ||
    !webdriverGetterNative
  );
  // Mirror CreepJS getPlatformEstimate BarcodeDetector split (Win vs Mac).
  const hasBarcodeDetector = 'BarcodeDetector' in window;
  const platformHint = (() => {
    const hasTouch = 'ontouchstart' in window && typeof TouchEvent !== 'undefined';
    const hasAppBadge = 'setAppBadge' in Navigator.prototype;
    const hasSharedWorker = 'SharedWorker' in window;
    const hasEyeDropper = 'EyeDropper' in window;
    const hasFsw = 'FileSystemWritableFileStream' in window;
    const hasHid = 'HID' in window && 'HIDDevice' in window;
    const hasSerial = 'SerialPort' in window && 'Serial' in window;
    const noDownlinkMax = !('downlinkMax' in (navigator.connection || {}));
    const v88 = CSS.supports('aspect-ratio: initial');
    const win = [
      v88 ? !hasBarcodeDetector : null,
      noDownlinkMax,
      hasEyeDropper,
      hasFsw,
      hasHid,
      hasSerial,
      hasSharedWorker,
      true,
      hasAppBadge,
    ].filter((x) => x !== null);
    const mac = [
      v88 ? hasBarcodeDetector : null,
      noDownlinkMax,
      hasEyeDropper,
      hasFsw,
      hasHid,
      hasSerial,
      hasSharedWorker,
      !hasTouch,
      hasAppBadge,
    ].filter((x) => x !== null);
    const score = (arr) => +(arr.filter(Boolean).length / arr.length).toFixed(2);
    return {
      hasBarcodeDetector,
      windows: score(win),
      mac: score(mac),
    };
  })();
  return {
    site: "creepjs",
    webdriver: navigator.webdriver,
    fingerprintApplied: !!globalThis[Symbol.for("bobby.fp.applied")],
    bodyText: text.slice(0, 8000),
    lieHints: text.match(/lie[s]?|headless|webdriver|stealth|inconsistenc|bot|worker|sharedworker/gi)?.slice(0, 40) || [],
    workerHeadlessLeak: text.toLowerCase().includes("headlesschrome"),
    headlessScores: {
      like: text.match(/(\d+)%\s*like headless/i)?.[1] || null,
      headless: text.match(/(\d+)%\s*headless/i)?.[1] || null,
      stealth: text.match(/(\d+)%\s*stealth/i)?.[1] || null,
    },
    headlessFlags: {
      webDriverIsOn,
      hasHeadlessUA: /HeadlessChrome/.test(navigator.userAgent) || /HeadlessChrome/.test(navigator.appVersion),
      webdriverGetterNative,
      prefersLightColor: matchMedia('(prefers-color-scheme: light)').matches,
    },
    stealthFlags: {
      hasToStringProxy,
      hasBadChromeRuntime,
    },
    platformHint,
    systemFonts: (() => {
      try {
        const el = document.createElement("div");
        document.body.appendChild(el);
        const families = new Set();
        ["caption", "icon", "menu", "message-box", "small-caption", "status-bar"].forEach((font) => {
          el.setAttribute("style", "font: " + font + " !important");
          families.add(getComputedStyle(el).fontFamily);
        });
        document.body.removeChild(el);
        return Array.from(families);
      } catch (_) {
        return [];
      }
    })(),
  };
})()"#,
            15_000,
        )
        .await;
        let worker_probe = eval_json(
            worker.as_ref(),
            &page_id,
            &build_worker_probe_script(),
            15_000,
        )
        .await;
        eprintln!(
            "=== CreepJS worker probe ===\n{}",
            serde_json::to_string_pretty(&worker_probe).unwrap()
        );
        if let Some(obj) = report.as_object_mut() {
            obj.insert("workerProbe".to_string(), worker_probe.clone());
        }
        if let Some(scores) = report.get("headlessScores") {
            eprintln!(
                "=== CreepJS headless scores ===\n  like headless: {}\n  headless: {}\n  stealth: {}",
                scores["like"].as_str().unwrap_or("n/a"),
                scores["headless"].as_str().unwrap_or("n/a"),
                scores["stealth"].as_str().unwrap_or("n/a"),
            );
        }
        eprintln!(
            "=== CreepJS flags ===\n{}",
            serde_json::to_string_pretty(&json!({
                "headlessFlags": report.get("headlessFlags"),
                "stealthFlags": report.get("stealthFlags"),
            }))
            .unwrap()
        );
        eprintln!(
            "=== CreepJS ===\n{}",
            serde_json::to_string_pretty(&report).unwrap()
        );
        soft_findings.extend(collect_soft_findings(&report));
        reports.push(report.clone());

        assert_eq!(
            report["headlessFlags"]["webDriverIsOn"], false,
            "CreepJS webDriverIsOn must be false"
        );
        assert_eq!(
            report["headlessFlags"]["hasHeadlessUA"], false,
            "CreepJS hasHeadlessUA must be false"
        );
        assert_eq!(
            report["stealthFlags"]["hasToStringProxy"], false,
            "CreepJS hasToStringProxy must be false"
        );
        assert_eq!(
            report["stealthFlags"]["hasBadChromeRuntime"], false,
            "CreepJS hasBadChromeRuntime must be false"
        );
        eprintln!(
            "=== CreepJS platform hint ===\n{}",
            serde_json::to_string_pretty(&report["platformHint"]).unwrap()
        );
        assert_eq!(
            report["platformHint"]["hasBarcodeDetector"], false,
            "Windows persona must hide BarcodeDetector"
        );
        let win = report["platformHint"]["windows"].as_f64().unwrap_or(0.0);
        let mac = report["platformHint"]["mac"].as_f64().unwrap_or(0.0);
        assert!(
            win >= mac,
            "Windows platform estimate ({win}) must beat or tie Mac ({mac})"
        );
        let system_fonts = report["systemFonts"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();
        eprintln!("=== CreepJS systemFonts probe ===\n{system_fonts:?}");
        assert!(
            system_fonts.iter().any(|f| f.contains("Segoe UI")),
            "system UI fonts must resolve to Segoe UI on Windows persona, got {system_fonts:?}"
        );
        assert!(
            !system_fonts
                .iter()
                .any(|f| *f == "Arial" || f.starts_with("Arial,")),
            "system UI fonts must not collapse to Arial, got {system_fonts:?}"
        );
        let worker_ua = worker_probe["worker"]["ua"].as_str().unwrap_or("");
        assert!(
            !worker_ua.to_ascii_lowercase().contains("headless"),
            "creepjs worker UA leaked headless: {worker_ua}"
        );
    }

    // C. FingerprintJS demo
    {
        let page_id = open_page(worker.as_ref()).await;
        navigate(
            worker.as_ref(),
            &page_id,
            "https://fingerprintjs.github.io/fingerprintjs/",
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let report = eval_json(
            worker.as_ref(),
            &page_id,
            r#"({
  site: "fingerprintjs",
  webdriver: navigator.webdriver,
  fingerprintApplied: !!globalThis[Symbol.for("bobby.fp.applied")],
  visitorId: (document.body?.innerText || "").match(/Visitor ID:\s*([a-f0-9]+)/i)?.[1] || null,
  bodySnippet: document.body?.innerText?.slice(0, 2500) || ""
})"#,
            15_000,
        )
        .await;
        eprintln!(
            "=== FingerprintJS ===\n{}",
            serde_json::to_string_pretty(&report).unwrap()
        );
        soft_findings.extend(collect_soft_findings(&report));
        reports.push(report);
    }

    if !soft_findings.is_empty() {
        eprintln!("\n=== Soft findings (non-fatal) ===");
        for finding in &soft_findings {
            eprintln!("  - {finding}");
        }
    }

    for report in &reports {
        let site = report["site"].as_str().unwrap_or("unknown");
        assert_eq!(
            report["fingerprintApplied"], true,
            "{site}: fingerprint not applied"
        );
        assert!(
            report["webdriver"].is_null() || report["webdriver"] == false,
            "{site}: webdriver tell detected: {:?}",
            report["webdriver"]
        );
    }

    let browserleaks = reports
        .iter()
        .find(|r| r["site"] == "browserleaks-js")
        .expect("browserleaks report");
    assert_eq!(browserleaks["chrome"], true, "chrome object missing");
    // Real Chrome may omit chrome.runtime under CDP; we intentionally do not
    // inject a runtime stub (CreepJS hasBadChromeRuntime).

    let creepjs = reports
        .iter()
        .find(|r| r["site"] == "creepjs")
        .expect("creepjs report");
    let body_text = creepjs["bodyText"].as_str().unwrap_or("");
    assert!(
        !body_text.is_empty(),
        "creepjs: page body empty — page did not load"
    );
    if let Some(worker_probe) = creepjs.get("workerProbe") {
        let session = fingerprinting::create_session(
            &FingerprintConfig::default()
                .with_enabled(true)
                .with_session_seed(777)
                .with_inject_chrome(true),
        );
        let worker_ua = worker_probe["worker"]["ua"].as_str().unwrap_or("");
        assert_eq!(worker_ua, session.user_agent, "creepjs worker UA mismatch");
        assert!(
            !worker_ua.contains("HeadlessChrome"),
            "creepjs worker UA leaked headless"
        );
    }
}
