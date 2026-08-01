//! Fingerprinting masking module for browser automation.
//!
//! Provides Canvas/WebGL hash generation, AudioContext masking, font
//! enumeration standardization, and screen resolution spoofing to
//! evade bot detection systems.

mod audio;
mod canvas;
mod fonts;
mod screen;

pub use audio::{AudioConfig, AudioMasker};
pub use canvas::{CanvasConfig, CanvasMasker};
pub use fonts::{FontConfig, FontMasker};
pub use screen::{ScreenConfig, ScreenMasker};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

/// Configuration for the fingerprinting mask.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FingerprintConfig {
    pub canvas: CanvasConfig,
    pub audio: AudioConfig,
    pub fonts: FontConfig,
    pub screen: ScreenConfig,
    #[serde(default = "default_session_seed")]
    pub session_seed: u64,
}

impl Default for FingerprintConfig {
    fn default() -> Self {
        Self {
            canvas: CanvasConfig::default(),
            audio: AudioConfig::default(),
            fonts: FontConfig::default(),
            screen: ScreenConfig::default(),
            session_seed: default_session_seed(),
        }
    }
}

fn default_session_seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_nanos();
    (nanos as u64).wrapping_mul(2654435761)
}

impl FingerprintConfig {
    pub fn with_canvas(mut self, config: CanvasConfig) -> Self {
        self.canvas = config;
        self
    }

    pub fn with_audio(mut self, config: AudioConfig) -> Self {
        self.audio = config;
        self
    }

    pub fn with_fonts(mut self, config: FontConfig) -> Self {
        self.fonts = config;
        self
    }

    pub fn with_screen(mut self, config: ScreenConfig) -> Self {
        self.screen = config;
        self
    }

    pub fn with_session_seed(mut self, seed: u64) -> Self {
        self.session_seed = seed;
        self
    }
}

/// A complete fingerprint session with consistent masks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FingerprintSession {
    pub session_id: String,
    pub canvas_hash: String,
    pub webgl_hash: String,
    pub audio_hash: String,
    pub font_list: Vec<String>,
    pub screen_resolution: ScreenResolution,
    pub user_agent: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenResolution {
    pub width: u32,
    pub height: u32,
    pub available_width: u32,
    pub available_height: u32,
    pub color_depth: u32,
    pub pixel_ratio: f64,
}

impl Default for ScreenResolution {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            available_width: 1920,
            available_height: 1040,
            color_depth: 24,
            pixel_ratio: 1.0,
        }
    }
}

/// Generates a fingerprinting session from configuration.
pub fn create_session(config: &FingerprintConfig) -> FingerprintSession {
    let _rng = StdRng::seed_from_u64(config.session_seed);

    // Generate consistent canvas hash
    let canvas_masker = CanvasMasker::new(config.canvas.clone());
    let canvas_hash = canvas_masker.generate_hash(StdRng::seed_from_u64(config.session_seed));

    // Generate consistent WebGL hash
    let webgl_hash = generate_webgl_hash(&canvas_hash, StdRng::seed_from_u64(config.session_seed.wrapping_add(1)));

    // Generate consistent audio hash
    let audio_masker = AudioMasker::new(config.audio.clone());
    let audio_hash = audio_masker.generate_hash(StdRng::seed_from_u64(config.session_seed.wrapping_add(2)));

    // Get standardized font list
    let font_masker = FontMasker::new(config.fonts.clone());
    let font_list = font_masker.get_standard_fonts();

    // Get spoofed screen resolution
    let screen_masker = ScreenMasker::new(config.screen.clone());
    let screen_res = screen_masker.get_spoofed_resolution();

    FingerprintSession {
        session_id: format!("fp_{}", config.session_seed),
        canvas_hash,
        webgl_hash,
        audio_hash,
        font_list,
        screen_resolution: screen_res.clone(),
        user_agent: generate_user_agent(&screen_res),
    }
}

fn generate_webgl_hash(canvas_hash: &str, mut rng: StdRng) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canvas_hash.as_bytes());
    let random_bytes: [u8; 32] = rng.random();
    hasher.update(&random_bytes);
    hex::encode(hasher.finalize())
}

fn generate_user_agent(_res: &ScreenResolution) -> String {
    format!(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
    )
}
