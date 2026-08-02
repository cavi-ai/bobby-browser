//! Conformance probes for fingerprint apply plans (no live browser required).

use fingerprinting::{
    build_collector_probe_script, build_probe_script, create_session, FingerprintApplyPlan,
    FingerprintConfig, ScreenConfig, INIT_SCRIPT_TEMPLATE,
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
    assert!(script.contains("toStringNativeWebdriver"));
    assert!(script.contains("pdfViewerEnabled"));
    assert!(script.contains("vendorGoogle"));
    assert!(script.contains("canvasStable"));
}

#[test]
fn init_script_contains_worker_wrappers() {
    assert!(INIT_SCRIPT_TEMPLATE.contains("importScripts"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("SharedWorker"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("workerBootstrap"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("wrapWorkerScriptUrl"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("bobby.fp.worker"));
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
    assert!(INIT_SCRIPT_TEMPLATE.contains("rewriteFontFamilyList"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("patchMeasureText"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("FontFace.prototype.load"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("TouchEvent"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("--any-pointer"));
    assert!(INIT_SCRIPT_TEMPLATE.contains("CSSStyleDeclaration"));
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
    let embedded = plan
        .init_script
        .find("const P = ")
        .and_then(|idx| {
            let rest = &plan.init_script[idx + "const P = ".len()..];
            let end = rest.find(";\n")?;
            Some(&rest[..end])
        })
        .expect("embedded profile JSON");
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
