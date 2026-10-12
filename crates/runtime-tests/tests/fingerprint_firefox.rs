//! Live Firefox collector probes against public fingerprint pages.
//!
//! Env: `BOBBY_FIREFOX_BIN`, `BOBBY_FIREFOX_PROFILE`, `BOBBY_COMPANION_EXTENSION`.

use std::time::{Duration, Instant};

use fingerprinting::build_worker_probe_script;
use runtime_tests::{with_installed_firefox_worker, InstalledFirefoxConfig};
use types::{
    CommandError, ErrorCode, ErrorLayer, EvaluateJavaScriptCommand, Evidence, NavigateCommand,
    PageId, WaitUntil,
};
use worker_pool::BrowserWorker;

fn workflow_error(code: ErrorCode, error: impl std::fmt::Display) -> CommandError {
    CommandError {
        code,
        message: error.to_string(),
        layer: ErrorLayer::Driver,
        retryable: false,
    }
}

#[derive(Debug, Clone)]
pub struct FirefoxFingerprintDogfoodReport {
    pub reports: Vec<serde_json::Value>,
    pub soft_findings: Vec<String>,
}

async fn run_firefox_collector_probes(
    worker: &dyn BrowserWorker,
) -> Result<FirefoxFingerprintDogfoodReport, CommandError> {
    let mut reports: Vec<serde_json::Value> = Vec::new();
    let mut soft_findings: Vec<String> = Vec::new();

    // A. BrowserLeaks JS
    {
        let page_id = PageId::new();
        worker_pool::tabs_or_default(worker.tabs())
            .open_page(page_id.clone())
            .await?;
        worker_pool::navigation_or_default(worker.navigation())
            .navigate(
                &page_id,
                &NavigateCommand {
                    url: "https://browserleaks.com/javascript".into(),
                    wait_until: WaitUntil::Interactive,
                    timeout_ms: 30_000,
                },
            )
            .await?;
        tokio::time::sleep(Duration::from_secs(3)).await;
        let report = firefox_eval_json(
            worker,
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
        .await?;
        eprintln!(
            "=== [Firefox] BrowserLeaks JS ===\n{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
        soft_findings.extend(firefox_collect_soft_findings(&report));
        if report["chrome"] == true {
            soft_findings.push(
                "browserleaks-js: unexpected chrome object on Firefox (inject_chrome is false)"
                    .into(),
            );
        }
        reports.push(report);
    }

    // B. CreepJS — finishes once getComputedStyle Proxy is skipped on Gecko.
    {
        let page_id = PageId::new();
        worker_pool::tabs_or_default(worker.tabs())
            .open_page(page_id.clone())
            .await?;
        worker_pool::navigation_or_default(worker.navigation())
            .navigate(
                &page_id,
                &NavigateCommand {
                    url: "https://abrahamjuliot.github.io/creepjs/".into(),
                    wait_until: WaitUntil::Interactive,
                    timeout_ms: 30_000,
                },
            )
            .await?;
        let creepjs_ready = firefox_wait_for_body(
            worker,
            &page_id,
            r#"(function() {
  const t = document.body?.innerText || "";
  if (t.includes("Computing...")) return false;
  return t.includes("FP ID") || t.includes("like headless") || t.includes("% headless") || t.length > 1200;
})()"#,
            45_000,
            1500,
        )
        .await;

        if let Err(error) = creepjs_ready {
            soft_findings.push(format!(
                "creepjs: page wait failed ({}); probing partial DOM",
                error.message
            ));
        }
        let mut report = firefox_eval_json(worker, &page_id, FIREFOX_CREEPJS_PROBE, 15_000).await?;
        let worker_probe =
            match firefox_eval_json(worker, &page_id, &build_worker_probe_script(), 15_000).await {
                Ok(probe) => probe,
                Err(error) => {
                    soft_findings.push(format!("creepjs: worker probe failed ({})", error.message));
                    serde_json::json!({"error": error.message})
                }
            };
        eprintln!(
            "=== [Firefox] CreepJS worker probe ===\n{}",
            serde_json::to_string_pretty(&worker_probe).unwrap_or_default()
        );
        if let Some(obj) = report.as_object_mut() {
            obj.insert("workerProbe".to_string(), worker_probe);
        }
        if let Some(scores) = report.get("headlessScores") {
            eprintln!(
                "=== [Firefox] CreepJS headless scores ===\n  like headless: {}\n  headless: {}\n  stealth: {}",
                scores["like"].as_str().unwrap_or("n/a"),
                scores["headless"].as_str().unwrap_or("n/a"),
                scores["stealth"].as_str().unwrap_or("n/a"),
            );
        }
        eprintln!(
            "=== [Firefox] CreepJS flags ===\n{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "headlessFlags": report.get("headlessFlags"),
                "stealthFlags": report.get("stealthFlags"),
            }))
            .unwrap_or_default()
        );
        eprintln!(
            "=== [Firefox] CreepJS ===\n{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
        soft_findings.extend(firefox_collect_soft_findings(&report));
        if report
            .pointer("/stealthFlags/hasBadChromeRuntime")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            soft_findings.push("creepjs: hasBadChromeRuntime (unexpected on Firefox)".into());
        }
        let body = report["bodyText"].as_str().unwrap_or("");
        if body.contains("Computing...") {
            return Err(workflow_error(
                ErrorCode::VerificationFailed,
                "CreepJS stuck on Computing... (Gecko getComputedStyle Proxy must stay disabled)",
            ));
        }
        if report["headlessScores"]["like"].as_str().is_none()
            || report["headlessScores"]["headless"].as_str().is_none()
            || report["headlessScores"]["stealth"].as_str().is_none()
        {
            return Err(workflow_error(
                ErrorCode::VerificationFailed,
                "CreepJS headless scores missing after collection",
            ));
        }
        reports.push(report);
    }

    // C. FingerprintJS demo
    {
        let page_id = PageId::new();
        worker_pool::tabs_or_default(worker.tabs())
            .open_page(page_id.clone())
            .await?;
        worker_pool::navigation_or_default(worker.navigation())
            .navigate(
                &page_id,
                &NavigateCommand {
                    url: "https://fingerprintjs.github.io/fingerprintjs/".into(),
                    wait_until: WaitUntil::Interactive,
                    timeout_ms: 30_000,
                },
            )
            .await?;
        tokio::time::sleep(Duration::from_secs(5)).await;
        let report = firefox_eval_json(
            worker,
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
        .await?;
        eprintln!(
            "=== [Firefox] FingerprintJS ===\n{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
        soft_findings.extend(firefox_collect_soft_findings(&report));
        reports.push(report);
    }

    for report in &reports {
        let site = report["site"].as_str().unwrap_or("unknown");
        if report["fingerprintApplied"] != true {
            return Err(workflow_error(
                ErrorCode::VerificationFailed,
                format!("{site}: fingerprint not applied"),
            ));
        }
        if !(report["webdriver"].is_null() || report["webdriver"] == false) {
            return Err(workflow_error(
                ErrorCode::VerificationFailed,
                format!("{site}: webdriver tell detected: {:?}", report["webdriver"]),
            ));
        }
    }

    let creepjs = reports
        .iter()
        .find(|r| r["site"] == "creepjs")
        .ok_or_else(|| workflow_error(ErrorCode::VerificationFailed, "creepjs report missing"))?;
    if creepjs.pointer("/headlessFlags/webDriverIsOn") != Some(&serde_json::Value::Bool(false)) {
        return Err(workflow_error(
            ErrorCode::VerificationFailed,
            "CreepJS webDriverIsOn must be false",
        ));
    }
    if creepjs
        .pointer("/headlessFlags/webdriverGetterNative")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        soft_findings.push("creepjs: webdriver getter not native-looking after Proxy patch".into());
    }
    if creepjs.pointer("/headlessFlags/hasHeadlessUA") != Some(&serde_json::Value::Bool(false)) {
        return Err(workflow_error(
            ErrorCode::VerificationFailed,
            "CreepJS hasHeadlessUA must be false",
        ));
    }
    if creepjs.pointer("/stealthFlags/hasToStringProxy") != Some(&serde_json::Value::Bool(false)) {
        return Err(workflow_error(
            ErrorCode::VerificationFailed,
            "CreepJS hasToStringProxy must be false",
        ));
    }
    if creepjs.pointer("/headlessFlags/prefersLightColor") != Some(&serde_json::Value::Bool(false))
    {
        soft_findings.push("creepjs: prefersLightColor still true after dark scheme patch".into());
    }
    if creepjs.pointer("/platformHint/hasBarcodeDetector") != Some(&serde_json::Value::Bool(false))
    {
        return Err(workflow_error(
            ErrorCode::VerificationFailed,
            "Windows persona must hide BarcodeDetector",
        ));
    }
    let win = creepjs["platformHint"]["windows"].as_f64().unwrap_or(0.0);
    let mac = creepjs["platformHint"]["mac"].as_f64().unwrap_or(0.0);
    if win < mac {
        soft_findings.push(format!(
            "creepjs: Windows platform estimate ({win}) behind Mac ({mac}) — Gecko API lean expected"
        ));
    }
    let system_fonts = creepjs["systemFonts"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();
    eprintln!("=== [Firefox] CreepJS systemFonts probe ===\n{system_fonts:?}");
    if !system_fonts.iter().any(|f| {
        let n = f.trim_matches('"');
        n.contains("Segoe UI") || n == "Tahoma"
    }) {
        return Err(workflow_error(
            ErrorCode::VerificationFailed,
            format!(
                "creepjs: system UI fonts missing Windows family (Segoe UI/Tahoma), got {system_fonts:?}"
            ),
        ));
    }

    if !soft_findings.is_empty() {
        eprintln!("\n=== [Firefox] Soft findings (non-fatal) ===");
        for finding in &soft_findings {
            eprintln!("  - {finding}");
        }
    }

    Ok(FirefoxFingerprintDogfoodReport {
        reports,
        soft_findings,
    })
}

const FIREFOX_CREEPJS_PROBE: &str = r#"(async () => {
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
      hasIframeProxy: (() => {
        try {
          const iframe = document.createElement('iframe');
          iframe.srcdoc = 'probe';
          return !!iframe.contentWindow === false;
        } catch (err) { return true; }
      })(),
      hasHighChromeIndex: (() => {
        try {
          return Object.keys(window).slice(-50).includes('chrome') &&
            Object.getOwnPropertyNames(window).slice(-50).includes('chrome');
        } catch (_) { return false; }
      })(),
    },
    likeHeadlessFlags: {
      noChrome: !('chrome' in window),
      noPlugins: navigator.plugins ? navigator.plugins.length === 0 : true,
      notificationIsDenied: ('Notification' in window) && Notification.permission === 'denied',
      prefersLightColor: matchMedia('(prefers-color-scheme: light)').matches,
      pdfIsDisabled: ('pdfViewerEnabled' in navigator) && navigator.pdfViewerEnabled === false,
      noTaskbar: screen.height === screen.availHeight && screen.width === screen.availWidth,
      hasVvpScreenRes: (innerWidth === screen.width && outerHeight === screen.height),
      noWebShare: !('share' in navigator) || !('canShare' in navigator),
      noContentIndex: !('ContentIndex' in window),
      noContactsManager: !('ContactsManager' in window),
      noDownlinkMax: !('downlinkMax' in (navigator.connection || {})),
    },
    pageGpu: (() => {
      try {
        const canvas = document.createElement('canvas');
        const gl = canvas.getContext('webgl') || canvas.getContext('webgl2');
        if (!gl) return null;
        const ext = gl.getExtension('WEBGL_debug_renderer_info');
        return gl.getParameter(ext ? ext.UNMASKED_RENDERER_WEBGL : gl.RENDERER);
      } catch (_) { return null; }
    })(),
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
})()"#;

async fn firefox_eval_json(
    worker: &dyn BrowserWorker,
    page_id: &PageId,
    expression: &str,
    timeout_ms: u64,
) -> Result<serde_json::Value, CommandError> {
    let evidence = worker_pool::javascript_or_default(worker.javascript())
        .evaluate_javascript(
            page_id,
            &EvaluateJavaScriptCommand {
                expression: expression.to_owned(),
                timeout_ms,
                await_promise: true,
            },
        )
        .await?;
    evidence
        .into_iter()
        .find_map(|item| match item {
            Evidence::JavaScriptResult { value, .. } => Some(value),
            _ => None,
        })
        .ok_or_else(|| {
            workflow_error(
                ErrorCode::VerificationFailed,
                "Firefox evaluate_javascript missing JavaScriptResult evidence",
            )
        })
}

async fn firefox_wait_for_body(
    worker: &dyn BrowserWorker,
    page_id: &PageId,
    predicate: &str,
    timeout_ms: u64,
    poll_ms: u64,
) -> Result<(), CommandError> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut last_len = 0usize;
    loop {
        let ready = firefox_eval_json(worker, page_id, &format!("!!({predicate})"), 5_000)
            .await
            .unwrap_or(serde_json::Value::Bool(false));
        if ready.as_bool() == Some(true) {
            return Ok(());
        }
        if let Ok(snippet) = firefox_eval_json(
            worker,
            page_id,
            r#"((document.body && document.body.innerText) || "").slice(0, 400)"#,
            5_000,
        )
        .await
        {
            let text = snippet.as_str().unwrap_or("");
            if text.len() != last_len {
                eprintln!(
                    "=== [Firefox] collector wait body ({len} chars) ===\n{text}",
                    len = text.len()
                );
                last_len = text.len();
            }
        }
        if Instant::now() >= deadline {
            return Err(workflow_error(
                ErrorCode::DeadlineExceeded,
                "Firefox collector page body wait timed out",
            ));
        }
        tokio::time::sleep(Duration::from_millis(poll_ms)).await;
    }
}

fn firefox_collect_soft_findings(report: &serde_json::Value) -> Vec<String> {
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
#[ignore = "requires installed headed Firefox + companion profile (BOBBY_FIREFOX_*)"]
async fn installed_firefox_fingerprint_collector_dogfood_passes() {
    let config = InstalledFirefoxConfig::from_env().expect("installed Firefox test configuration");
    let report = with_installed_firefox_worker(config, |worker| {
        Box::pin(run_firefox_collector_probes(worker))
    })
    .await
    .expect("Firefox fingerprint collector dogfood");

    let creepjs = report
        .reports
        .iter()
        .find(|r| r["site"] == "creepjs")
        .expect("creepjs report");
    eprintln!(
        "Firefox CreepJS scores: like={:?} headless={:?} stealth={:?} webDriverIsOn={:?} prefersLight={:?}",
        creepjs["headlessScores"]["like"],
        creepjs["headlessScores"]["headless"],
        creepjs["headlessScores"]["stealth"],
        creepjs["headlessFlags"]["webDriverIsOn"],
        creepjs["headlessFlags"]["prefersLightColor"],
    );
    eprintln!(
        "Firefox like-headless flags: {}",
        serde_json::to_string_pretty(&creepjs["likeHeadlessFlags"]).unwrap_or_default()
    );
    eprintln!(
        "Firefox stealth flags: {}",
        serde_json::to_string_pretty(&creepjs["stealthFlags"]).unwrap_or_default()
    );
    eprintln!(
        "Firefox GPU page={:?} worker={:?}",
        creepjs["pageGpu"],
        creepjs.pointer("/workerProbe/worker/webglRenderer")
    );
    assert_eq!(
        creepjs["headlessFlags"]["webDriverIsOn"], false,
        "webDriverIsOn must be false"
    );
    assert_eq!(
        creepjs["headlessFlags"]["hasHeadlessUA"], false,
        "hasHeadlessUA must be false"
    );
    assert_eq!(
        creepjs["stealthFlags"]["hasToStringProxy"], false,
        "hasToStringProxy must be false"
    );
    assert!(
        creepjs["webdriver"].is_null() || creepjs["webdriver"] == false,
        "navigator.webdriver value must be false"
    );
}
