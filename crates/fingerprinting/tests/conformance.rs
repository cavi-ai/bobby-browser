//! Conformance probes for fingerprint apply plans (no live browser required).

use fingerprinting::{
    build_probe_script, create_session, FingerprintApplyPlan, FingerprintConfig, ScreenConfig,
};
use serde_json::Value;

#[test]
fn probe_script_is_async_iife() {
    let script = build_probe_script();
    assert!(script.contains("canvasHash"));
    assert!(script.contains("webglVendor"));
    assert!(script.contains("fingerprintApplied"));
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
    assert_eq!(plan.device_metrics.device_scale_factor, session.screen_resolution.pixel_ratio);

    assert!(plan.init_script.contains(&session.webgl.vendor));
    assert!(plan.init_script.contains(&session.webgl.renderer));
    assert!(plan.init_script.contains(&session.user_agent));
    assert!(plan.init_script.contains("hardwareConcurrency"));
    assert!(plan.init_script.contains("__bobbyFingerprintApplied"));
}

#[test]
fn session_json_embeds_in_init_script() {
    let session = create_session(&FingerprintConfig::default().with_session_seed(7));
    let plan = FingerprintApplyPlan::from_session(session.clone());
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
}

#[test]
fn toggle_off_skips_plan() {
    let config = FingerprintConfig::default()
        .with_session_seed(1)
        .with_enabled(false);
    assert!(FingerprintApplyPlan::from_config(&config).unwrap().is_none());
}
