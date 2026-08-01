use fingerprinting::{
    build_init_script, build_probe_script, CanvasConfig, CanvasMasker, FingerprintApplyPlan,
    FingerprintConfig, FontConfig, FontMasker, ScreenConfig, ScreenMasker, WebGlConfig,
    create_session,
};
use rand::SeedableRng;

#[test]
fn canvas_masker_consistent_hash() {
    let config = CanvasConfig::default();
    let masker = CanvasMasker::new(config);
    let hash1 = masker.generate_hash(rand::rngs::StdRng::seed_from_u64(1));
    let hash2 = masker.generate_hash(rand::rngs::StdRng::seed_from_u64(1));
    assert_eq!(hash1, hash2);
}

#[test]
fn canvas_masker_different_configs_different_hashes() {
    let masker1 = CanvasMasker::new(CanvasConfig::default());
    let masker2 = CanvasMasker::new(
        CanvasConfig::default()
            .with_hash_seed(9999)
            .with_noise_key("other"),
    );
    let hash1 = masker1.generate_hash(rand::rngs::StdRng::seed_from_u64(1));
    let hash2 = masker2.generate_hash(rand::rngs::StdRng::seed_from_u64(1));
    assert_ne!(hash1, hash2);
}

#[test]
fn canvas_hash_is_hex() {
    let masker = CanvasMasker::new(CanvasConfig::default());
    let hash = masker.generate_hash(rand::rngs::StdRng::seed_from_u64(42));
    assert_eq!(hash.len(), 64);
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn font_masker_returns_standard_fonts() {
    let masker = FontMasker::new(FontConfig::default());
    let fonts = masker.get_standard_fonts();
    assert!(fonts.len() >= 10);
    assert!(fonts.contains(&"Arial".to_string()));
    assert!(!fonts.iter().any(|f| f == "Helvetica"));
}

#[test]
fn font_masker_empty_falls_back_to_defaults() {
    let masker = FontMasker::new(FontConfig::default().with_standard_fonts(vec![]));
    assert!(!masker.get_standard_fonts().is_empty());
}

#[test]
fn font_masker_custom_fonts() {
    let masker = FontMasker::new(
        FontConfig::default().with_standard_fonts(vec!["CustomFont".to_string()]),
    );
    assert_eq!(masker.get_standard_fonts(), vec!["CustomFont".to_string()]);
}

#[test]
fn screen_masker_default_resolution() {
    let masker = ScreenMasker::new(ScreenConfig::default());
    let res = masker.get_spoofed_resolution();
    assert_eq!(res.width, 1920);
    assert_eq!(res.height, 1080);
    assert_eq!(res.color_depth, 24);
    assert!((res.pixel_ratio - 1.0).abs() < 0.01);
    assert!(res.available_height < res.height);
}

#[test]
fn screen_masker_custom_resolution() {
    let masker = ScreenMasker::new(
        ScreenConfig::default()
            .with_width(1280)
            .with_height(720)
            .with_color_depth(16)
            .with_pixel_ratio(2.0),
    );
    let res = masker.get_spoofed_resolution();
    assert_eq!(res.width, 1280);
    assert_eq!(res.height, 720);
    assert_eq!(res.color_depth, 16);
    assert!((res.pixel_ratio - 2.0).abs() < 0.01);
}

#[test]
fn create_session_produces_valid_session() {
    let config = FingerprintConfig::default().with_session_seed(42);
    let session = create_session(&config);
    assert_eq!(session.session_id, "fp_42");
    assert_eq!(session.canvas_hash.len(), 64);
    assert_eq!(session.webgl.hash.len(), 64);
    assert!(!session.font_list.is_empty());
    assert_eq!(session.screen_resolution.width, 1920);
    assert!(session.user_agent.contains("Chrome/131"));
    assert!(session.validate_consistency().is_ok());
}

#[test]
fn create_session_consistent() {
    let config = FingerprintConfig::default().with_session_seed(7);
    let session1 = create_session(&config);
    let session2 = create_session(&config);
    assert_eq!(session1, session2);
}

#[test]
fn create_session_different_seeds_different_hashes() {
    let config1 = FingerprintConfig::default().with_session_seed(111);
    let config2 = FingerprintConfig::default().with_session_seed(222);
    let session1 = create_session(&config1);
    let session2 = create_session(&config2);
    assert_ne!(session1.canvas_hash, session2.canvas_hash);
    assert_ne!(session1.webgl.hash, session2.webgl.hash);
    assert_ne!(session1.audio_hash, session2.audio_hash);
}

#[test]
fn apply_plan_none_when_disabled() {
    let config = FingerprintConfig::default().with_enabled(false);
    let plan = FingerprintApplyPlan::from_config(&config).unwrap();
    assert!(plan.is_none());
}

#[test]
fn apply_plan_contains_init_and_probe_scripts() {
    let config = FingerprintConfig::default().with_session_seed(99);
    let plan = FingerprintApplyPlan::from_config(&config).unwrap().unwrap();
    assert!(plan.init_script.contains("__bobbyFingerprintApplied"));
    assert!(plan.init_script.contains(&plan.session.user_agent));
    assert!(plan.init_script.contains(&plan.session.webgl.vendor));
    assert!(plan.probe_script.contains("canvasHash"));
    assert_eq!(plan.user_agent, plan.session.user_agent);
    assert_eq!(plan.device_metrics.width, 1920);
}

#[test]
fn init_script_is_deterministic_for_seed() {
    let config = FingerprintConfig::default().with_session_seed(55);
    let a = build_init_script(&create_session(&config));
    let b = build_init_script(&create_session(&config));
    assert_eq!(a, b);
    assert!(!build_probe_script().is_empty());
}

#[test]
fn consistency_rejects_mac_fonts_on_windows_ua() {
    let mut session = create_session(&FingerprintConfig::default().with_session_seed(1));
    session.font_list.push("Helvetica".to_string());
    assert!(session.validate_consistency().is_err());
}

#[test]
fn fingerprint_config_chain() {
    let config = FingerprintConfig::default()
        .with_session_seed(3)
        .with_canvas(CanvasConfig::default().with_hash_seed(42))
        .with_webgl(WebGlConfig::default().with_vendor("Test Vendor"))
        .with_fonts(FontConfig::default().with_standard_fonts(vec!["Test".to_string()]))
        .with_screen(
            ScreenConfig::default()
                .with_width(1366)
                .with_height(768),
        );
    let session = create_session(&config);
    assert_eq!(session.screen_resolution.width, 1366);
    assert_eq!(session.screen_resolution.height, 768);
    assert_eq!(session.font_list, vec!["Test".to_string()]);
    assert_eq!(session.webgl.vendor, "Test Vendor");
}

#[test]
fn serde_round_trip_session() {
    let session = create_session(&FingerprintConfig::default().with_session_seed(12));
    let json = serde_json::to_string(&session).unwrap();
    let back: fingerprinting::FingerprintSession = serde_json::from_str(&json).unwrap();
    assert_eq!(session, back);
}
