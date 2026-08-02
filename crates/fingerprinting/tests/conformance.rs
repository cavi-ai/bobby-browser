//! Conformance probes for fingerprint apply plans (no live browser required).

use fingerprinting::{
    build_collector_probe_script, build_probe_script, create_session, FingerprintApplyPlan,
    FingerprintConfig, ScreenConfig, INIT_SCRIPT_TEMPLATE, WORKER_BOOTSTRAP_TEMPLATE,
};
use serde_json::Value;

#[test]
fn probe_script_is_async_iife() {
    let script = build_probe_script();
    assert!(script.contains("canvasHash"));
    assert!(script.contains("webglVendor"));
    assert!(script.contains("fingerprintApplied"));
    assert!(script.contains("userAgentData"));
    assert!(script.contains("pluginCount"));
    assert!(script.contains("timezone"));
    assert!(script.contains("rtcConstructible"));
    assert!(script.contains("mediaDeviceCount"));
    assert!(script.contains("speechVoiceCount"));
    assert!(script.contains("batteryLevel"));
    assert!(script.contains("connectionEffectiveType"));
    assert!(script.contains("webglMaxTextureSize"));
}

#[test]
fn collector_probe_script_contains_detection_tells() {
    let script = build_collector_probe_script();
    assert!(script.contains("failCount"));
    assert!(script.contains("toStringNativeProto"));
    assert!(script.contains("pdfViewerEnabled"));
    assert!(script.contains("vendorGoogle"));
    assert!(script.contains("canvasStable"));
}

#[test]
fn init_script_contains_worker_wrappers() {
    assert!(INIT_SCRIPT_TEMPLATE.contains("importScripts"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("SharedWorker"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("getWorkerBootstrap"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("wrapWorkerScriptUrl"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("__BOBBY_WORKER_BOOTSTRAP__"));
    assert!(WORKER_BOOTSTRAP_TEMPLATE.contains("bobby.fp.worker"));
}

#[test]
fn init_script_contains_niche_surface_markers() {
    assert!(INIT_SCRIPT_TEMPLATE.contains("nativeFns") || INIT_SCRIPT_TEMPLATE.contains("cloak"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("pdfViewerEnabled"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("Google Inc."));
    assert!(INIT_SCRIPT_TEMPLATE.contains("outerWidth"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("matchMedia"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("ActiveText"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("getComputedStyle"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("chrome.app"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("BarcodeDetector"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("Segoe UI"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("message-box"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("rewriteFontFamilyList"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("patchMeasureText"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("FontFace.prototype.load"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("TouchEvent"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("--any-pointer"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("CSSStyleDeclaration"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("prefers-color-scheme:dark"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("prefers-color-scheme:light"));
}

#[test]
fn init_script_contains_deeper_surface_markers() {
    let config = FingerprintConfig::default().with_session_seed(505);
    let plan = FingerprintApplyPlan::from_config(&config).unwrap().unwrap();
    assert!(plan.init_script.contains("enumerateDevices"));
    assert!(plan.init_script.contains("getBattery"));
    assert!(plan.init_script.contains("iceTransportPolicy"));
    assert!(plan.init_script.contains("maxTextureSize"));
    assert!(plan.init_script.contains("speechSynthesis"));
    assert!(plan.init_script.contains("effectiveType"));
}

#[test]
fn apply_plan_matches_session_cross_signals() {
    let config = FingerprintConfig::default()
        .with_session_seed(404)
        .with_screen(ScreenConfig::default().with_width(1440).with_height(900))
        .with_locale("en-GB")
        .with_timezone_id("Europe/London")
        .with_chrome_major(131);
    let plan = FingerprintApplyPlan::from_config(&config).unwrap().unwrap();
    let session = &plan.session;

    assert_eq!(plan.user_agent, session.user_agent);
    assert!(plan.user_agent.contains("Chrome/131"));
    assert_eq!(plan.locale, "en-GB");
    assert_eq!(plan.timezone_id, "Europe/London");
    assert_eq!(plan.device_metrics.width, 1440);
    assert_eq!(
        plan.device_metrics.device_scale_factor,
        session.screen_resolution.pixel_ratio
    );

    assert!(plan.init_script.contains(&session.webgl.vendor));
    assert!(plan.init_script.contains(&session.webgl.renderer));
    assert!(plan.init_script.contains(&session.user_agent));
    assert!(plan.init_script.contains("hardwareConcurrency"));
    assert!(plan
        .init_script
        .contains("Symbol.for(\"bobby.fp.applied\")"));
    assert!(!plan.init_script.contains("__bobbyFingerprintApplied"));
    assert!(plan.init_script.contains("userAgentData"));
    assert!(plan.init_script.contains("clientHints"));
    assert_eq!(session.client_hints.platform, "Windows");
    assert!(!session.client_hints.brands.is_empty());
}

#[test]
fn session_json_embeds_in_init_script() {
    let session = create_session(&FingerprintConfig::default().with_session_seed(7));
    let plan = FingerprintApplyPlan::from_session(session.clone()).unwrap();
    let marker = "const P=";
    let start = plan
        .init_script
        .find(marker)
        .expect("embedded profile marker")
        + marker.len();
    let rest = &plan.init_script[start..];
    assert_eq!(rest.as_bytes()[0], b'{');
    let mut depth = 0i32;
    let mut end = None;
    for (i, ch) in rest.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    let embedded = &rest[..end.expect("profile JSON object")];
    let value: Value = serde_json::from_str(embedded).expect("valid JSON profile");
    assert_eq!(value["sessionSeed"], session.session_seed);
    assert_eq!(value["userAgent"], session.user_agent);
    assert_eq!(value["webgl"]["vendor"], session.webgl.vendor);
    assert_eq!(
        value["screenResolution"]["width"],
        session.screen_resolution.width
    );
    assert_eq!(
        value["clientHints"]["platform"],
        session.client_hints.platform
    );
}

#[test]
fn toggle_off_skips_plan() {
    let config = FingerprintConfig::default()
        .with_session_seed(1)
        .with_enabled(false);
    assert!(FingerprintApplyPlan::from_config(&config)
        .unwrap()
        .is_none());
}

#[test]
fn emitted_script_drops_placeholders_and_embeds_worker_bootstrap() {
    use fingerprinting::build_init_script;
    let session = create_session(
        &FingerprintConfig::default()
            .with_session_seed(99)
            .with_platform("Win32"),
    );
    let script = build_init_script(&session).unwrap();
    assert!(!script.contains("__BOBBY_FP_PROFILE__"));
    assert!(!script.contains("__BOBBY_WORKER_BOOTSTRAP__"));
    assert!(!script.contains("__BOBBY_FP_WORKER_PROFILE__"));
    // Worker bootstrap is a JS string literal injected for getWorkerBootstrap().
    assert!(script.contains("bobby.fp.worker"));
    assert!(script.contains("getWorkerBootstrap"));
    assert!(
        script.len() < 40_000,
        "script grew to {} bytes (budget 40k)",
        script.len()
    );
}

#[test]
fn win_vs_mac_session_platform_coherence() {
    let win = create_session(
        &FingerprintConfig::default()
            .with_session_seed(3)
            .with_platform("Win32"),
    );
    let mac = create_session(
        &FingerprintConfig::default()
            .with_session_seed(4)
            .with_platform("MacIntel"),
    );
    assert_eq!(win.client_hints.platform, "Windows");
    assert_eq!(mac.client_hints.platform, "macOS");
    assert!(win.max_touch_points == 0);
    assert!(mac.max_touch_points == 0);
    let win_plan = FingerprintApplyPlan::from_session(win).unwrap();
    let mac_plan = FingerprintApplyPlan::from_session(mac).unwrap();
    assert!(win_plan.init_script.contains("Windows") || win_plan.init_script.contains("Win32"));
    assert!(mac_plan.init_script.contains("macOS") || mac_plan.init_script.contains("MacIntel"));
}
