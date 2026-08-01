use fingerprinting::{
    CanvasConfig, CanvasMasker, FingerprintConfig, FingerprintSession, FontConfig, FontMasker,
    ScreenConfig, ScreenMasker, create_session,
};
use rand::SeedableRng;

#[test]
fn canvas_masker_consistent_hash() {
    let config = CanvasConfig::default();
    let masker = CanvasMasker::new(config);

    // Same config always produces the same hash
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
            .with_canvas_id("other".to_string()),
    );

    let hash1 = masker1.generate_hash(rand::rngs::StdRng::seed_from_u64(1));
    let hash2 = masker2.generate_hash(rand::rngs::StdRng::seed_from_u64(1));
    assert_ne!(hash1, hash2);
}

#[test]
fn canvas_hash_is_hex() {
    let masker = CanvasMasker::new(CanvasConfig::default());
    let hash = masker.generate_hash(rand::rngs::StdRng::seed_from_u64(42));

    // SHA-256 is 64 hex chars
    assert_eq!(hash.len(), 64);
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn font_masker_returns_standard_fonts() {
    let masker = FontMasker::new(FontConfig::default());
    let fonts = masker.get_standard_fonts();

    assert!(!fonts.is_empty());
    assert!(fonts.len() >= 10);
    assert!(fonts.contains(&"Arial".to_string()));
    assert!(fonts.contains(&"Verdana".to_string()));
}

#[test]
fn font_masker_custom_fonts() {
    let masker = FontMasker::new(
        FontConfig::default()
            .with_standard_fonts(vec!["CustomFont".to_string()]),
    );
    let fonts = masker.get_standard_fonts();
    assert_eq!(fonts, vec!["CustomFont".to_string()]);
}

#[test]
fn screen_masker_default_resolution() {
    let masker = ScreenMasker::new(ScreenConfig::default());
    let res = masker.get_spoofed_resolution();

    assert_eq!(res.width, 1920);
    assert_eq!(res.height, 1080);
    assert_eq!(res.color_depth, 24);
    assert!((res.pixel_ratio - 1.0).abs() < 0.01);
    // Available height should be less than total (taskbar)
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
    let config = FingerprintConfig::default();
    let session = create_session(&config);

    assert!(!session.session_id.is_empty());
    assert_eq!(session.session_id, format!("fp_{}", config.session_seed));
    assert_eq!(session.canvas_hash.len(), 64);
    assert_eq!(session.webgl_hash.len(), 64);
    assert!(!session.font_list.is_empty());
    assert_eq!(session.screen_resolution.width, 1920);
    assert!(session.user_agent.contains("Mozilla"));
}

#[test]
fn create_session_consistent() {
    let config = FingerprintConfig::default();
    let session1 = create_session(&config);
    let session2 = create_session(&config);

    assert_eq!(session1.canvas_hash, session2.canvas_hash);
    assert_eq!(session1.webgl_hash, session2.webgl_hash);
    assert_eq!(session1.audio_hash, session2.audio_hash);
    assert_eq!(session1.font_list, session2.font_list);
}

#[test]
fn create_session_different_seeds_different_hashes() {
    let config1 = FingerprintConfig::default().with_session_seed(111);
    let config2 = FingerprintConfig::default().with_session_seed(222);

    let session1 = create_session(&config1);
    let session2 = create_session(&config2);

    assert_ne!(session1.canvas_hash, session2.canvas_hash);
    assert_ne!(session1.webgl_hash, session2.webgl_hash);
    assert_ne!(session1.audio_hash, session2.audio_hash);
}

#[test]
fn fingerprint_config_chain() {
    let config = FingerprintConfig::default()
        .with_canvas(CanvasConfig::default().with_hash_seed(42))
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
}
