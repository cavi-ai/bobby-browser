//! Fingerprinting profiles and engine-agnostic apply plans.
//!
//! Generates a coherent session profile and an init script that masks
//! Canvas/WebGL/Audio/fonts/screen/navigator surfaces. Hosts (Chromium CDP,
//! Firefox BiDi, companion extensions) implement [`FingerprintHost`] to apply
//! the portable [`FingerprintApplyPlan`].

mod apply;
mod audio;
mod canvas;
mod error;
mod fonts;
mod screen;
mod script;
mod ua_ch;
mod webgl;

pub use apply::{DeviceMetrics, FingerprintApplyPlan, FingerprintHost};
pub use audio::{AudioConfig, AudioMasker};
pub use canvas::{CanvasConfig, CanvasMasker};
pub use error::FingerprintApplyError;
pub use fonts::{FontConfig, FontMasker};
pub use screen::{ScreenConfig, ScreenMasker};
pub use script::{
    build_collector_probe_script, build_font_probe_script, build_init_script, build_probe_script,
    build_worker_probe_script, INIT_SCRIPT_TEMPLATE, PROFILE_PLACEHOLDER,
    WORKER_BOOTSTRAP_PLACEHOLDER, WORKER_BOOTSTRAP_TEMPLATE, WORKER_PROFILE_PLACEHOLDER,
};
pub use ua_ch::{BrandVersion, ClientHintsProfile};
pub use webgl::{WebGlConfig, WebGlProfile};

use rand::rngs::StdRng;
use rand::SeedableRng;
use serde::{Deserialize, Serialize};

/// Configuration for the fingerprinting mask.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FingerprintConfig {
    /// Master on/off switch. When false, [`FingerprintApplyPlan::from_config`]
    /// returns `None` and hosts must skip injection.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub canvas: CanvasConfig,
    pub audio: AudioConfig,
    pub fonts: FontConfig,
    pub screen: ScreenConfig,
    #[serde(default)]
    pub webgl: WebGlConfig,
    #[serde(default = "default_locale")]
    pub locale: String,
    #[serde(default = "default_timezone")]
    pub timezone_id: String,
    #[serde(default = "default_platform")]
    pub platform: String,
    #[serde(default = "default_hardware_concurrency")]
    pub hardware_concurrency: u32,
    #[serde(default = "default_device_memory")]
    pub device_memory: u32,
    #[serde(default = "default_max_touch_points")]
    pub max_touch_points: u32,
    #[serde(default = "default_chrome_major")]
    pub chrome_major: u32,
    /// When true, init script injects a minimal `window.chrome` object (Chromium only).
    #[serde(default = "default_inject_chrome")]
    pub inject_chrome: bool,
    #[serde(default = "default_session_seed")]
    pub session_seed: u64,
}

impl Default for FingerprintConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            canvas: CanvasConfig::default(),
            audio: AudioConfig::default(),
            fonts: FontConfig::default(),
            screen: ScreenConfig::default(),
            webgl: WebGlConfig::default(),
            locale: default_locale(),
            timezone_id: default_timezone(),
            platform: default_platform(),
            hardware_concurrency: default_hardware_concurrency(),
            device_memory: default_device_memory(),
            max_touch_points: default_max_touch_points(),
            chrome_major: default_chrome_major(),
            inject_chrome: default_inject_chrome(),
            session_seed: default_session_seed(),
        }
    }
}

fn default_enabled() -> bool {
    true
}

fn default_locale() -> String {
    "en-US".to_string()
}

fn default_timezone() -> String {
    "America/New_York".to_string()
}

fn default_platform() -> String {
    "Win32".to_string()
}

fn default_hardware_concurrency() -> u32 {
    8
}

fn default_device_memory() -> u32 {
    8
}

fn default_max_touch_points() -> u32 {
    0
}

fn default_chrome_major() -> u32 {
    131
}

fn default_inject_chrome() -> bool {
    true
}

fn default_session_seed() -> u64 {
    0xB0B_5F1D_u64
}

impl FingerprintConfig {
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

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

    pub fn with_webgl(mut self, config: WebGlConfig) -> Self {
        self.webgl = config;
        self
    }

    pub fn with_locale(mut self, locale: impl Into<String>) -> Self {
        self.locale = locale.into();
        self
    }

    pub fn with_timezone_id(mut self, timezone_id: impl Into<String>) -> Self {
        self.timezone_id = timezone_id.into();
        self
    }

    pub fn with_session_seed(mut self, seed: u64) -> Self {
        self.session_seed = seed;
        self
    }

    pub fn with_chrome_major(mut self, major: u32) -> Self {
        self.chrome_major = major;
        self
    }

    pub fn with_inject_chrome(mut self, inject_chrome: bool) -> Self {
        self.inject_chrome = inject_chrome;
        self
    }

    pub fn with_platform(mut self, platform: impl Into<String>) -> Self {
        self.platform = platform.into();
        self
    }
}

/// A complete fingerprint session with consistent masks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FingerprintSession {
    pub session_id: String,
    pub session_seed: u64,
    pub canvas_hash: String,
    pub canvas_noise_amplitude: u8,
    pub webgl: WebGlProfile,
    pub audio_hash: String,
    pub audio_noise_scale: f64,
    pub font_list: Vec<String>,
    pub screen_resolution: ScreenResolution,
    pub user_agent: String,
    pub platform: String,
    pub locale: String,
    pub timezone_id: String,
    pub hardware_concurrency: u32,
    pub device_memory: u32,
    pub max_touch_points: u32,
    pub inject_chrome: bool,
    pub client_hints: ClientHintsProfile,
}

fn is_lowercase_hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

fn parse_chrome_major_from_ua(user_agent: &str) -> Option<u32> {
    user_agent
        .split("Chrome/")
        .nth(1)?
        .split('.')
        .next()?
        .parse()
        .ok()
}

fn parse_chrome_major_from_full_version(full_version: &str) -> Option<u32> {
    full_version.split('.').next()?.parse().ok()
}

fn expected_ch_platform(navigator_platform: &str) -> Option<&'static str> {
    match navigator_platform {
        "Win32" => Some("Windows"),
        "MacIntel" => Some("macOS"),
        "Linux x86_64" => Some("Linux"),
        _ => None,
    }
}

impl FingerprintSession {
    /// Fail closed when cross-signal profile fields contradict each other.
    pub fn validate_consistency(&self) -> Result<(), FingerprintApplyError> {
        match self.platform.as_str() {
            "Win32" => {
                if !self.user_agent.contains("Windows") {
                    return Err(FingerprintApplyError::Inconsistent(
                        "Win32 platform requires Windows in user-agent".into(),
                    ));
                }
                if self.user_agent.contains("Macintosh") {
                    return Err(FingerprintApplyError::Inconsistent(
                        "Win32 platform must not use Macintosh user-agent".into(),
                    ));
                }
            }
            "MacIntel" => {
                if !self.user_agent.contains("Macintosh") {
                    return Err(FingerprintApplyError::Inconsistent(
                        "MacIntel platform requires Macintosh in user-agent".into(),
                    ));
                }
                if self.user_agent.contains("Windows NT") {
                    return Err(FingerprintApplyError::Inconsistent(
                        "MacIntel platform must not use Windows NT user-agent".into(),
                    ));
                }
            }
            "Linux x86_64" => {
                if !self.user_agent.contains("Linux") {
                    return Err(FingerprintApplyError::Inconsistent(
                        "Linux x86_64 platform requires Linux in user-agent".into(),
                    ));
                }
            }
            other => {
                return Err(FingerprintApplyError::Inconsistent(format!(
                    "unsupported navigator platform: {other}"
                )));
            }
        }

        if let Some(expected) = expected_ch_platform(&self.platform) {
            if self.client_hints.platform != expected {
                return Err(FingerprintApplyError::Inconsistent(format!(
                    "client_hints.platform {expected} required for navigator platform {}",
                    self.platform
                )));
            }
        }

        if self.client_hints.brands.is_empty() {
            return Err(FingerprintApplyError::Inconsistent(
                "client_hints.brands must be non-empty".into(),
            ));
        }
        if self.client_hints.full_version_list.is_empty() {
            return Err(FingerprintApplyError::Inconsistent(
                "client_hints.full_version_list must be non-empty".into(),
            ));
        }
        if self.client_hints.full_version.is_empty() {
            return Err(FingerprintApplyError::Inconsistent(
                "client_hints.full_version must be non-empty".into(),
            ));
        }

        let ua_major = parse_chrome_major_from_ua(&self.user_agent).ok_or_else(|| {
            FingerprintApplyError::Inconsistent("user-agent must contain Chrome/{major}".into())
        })?;
        let ch_major = parse_chrome_major_from_full_version(&self.client_hints.full_version)
            .ok_or_else(|| {
                FingerprintApplyError::Inconsistent(
                    "client_hints.full_version must start with a Chrome major".into(),
                )
            })?;
        if ua_major != ch_major {
            return Err(FingerprintApplyError::Inconsistent(format!(
                "Chrome major mismatch: user-agent {ua_major} vs client_hints {ch_major}"
            )));
        }

        let brands_include_major = |brand: &str| {
            self.client_hints.brands.iter().any(|entry| {
                entry.brand == brand && entry.version.parse::<u32>().ok() == Some(ua_major)
            })
        };
        if !brands_include_major("Chromium") || !brands_include_major("Google Chrome") {
            return Err(FingerprintApplyError::Inconsistent(
                "client_hints.brands must include Chromium and Google Chrome with matching major"
                    .into(),
            ));
        }

        if self.max_touch_points == 0 && self.client_hints.mobile {
            return Err(FingerprintApplyError::Inconsistent(
                "desktop profile (max_touch_points=0) requires client_hints.mobile=false".into(),
            ));
        }

        if self.webgl.vendor.is_empty() || self.webgl.renderer.is_empty() {
            return Err(FingerprintApplyError::Inconsistent(
                "WebGL vendor and renderer must be non-empty".into(),
            ));
        }
        if !(1024..=32768).contains(&self.webgl.max_texture_size) {
            return Err(FingerprintApplyError::Inconsistent(
                "webgl.max_texture_size must be in 1024..=32768".into(),
            ));
        }

        if !(1..=256).contains(&self.hardware_concurrency) {
            return Err(FingerprintApplyError::Inconsistent(
                "hardware_concurrency must be in 1..=256".into(),
            ));
        }
        if !(1..=128).contains(&self.device_memory) {
            return Err(FingerprintApplyError::Inconsistent(
                "device_memory must be in 1..=128".into(),
            ));
        }

        if self.locale.is_empty() {
            return Err(FingerprintApplyError::Inconsistent(
                "locale must be non-empty".into(),
            ));
        }
        if self.timezone_id.is_empty() {
            return Err(FingerprintApplyError::Inconsistent(
                "timezone_id must be non-empty".into(),
            ));
        }
        if self.timezone_id != "UTC" && !self.timezone_id.contains('/') {
            return Err(FingerprintApplyError::Inconsistent(
                "timezone_id must be IANA-style (contain '/') or UTC".into(),
            ));
        }

        if self.user_agent.contains("Windows")
            && self
                .font_list
                .iter()
                .any(|font| font == "Helvetica" || font == "Menlo")
        {
            return Err(FingerprintApplyError::Inconsistent(
                "Windows profile must not advertise macOS-only fonts".into(),
            ));
        }

        let screen = &self.screen_resolution;
        if screen.width == 0 || screen.height == 0 {
            return Err(FingerprintApplyError::Inconsistent(
                "screen dimensions must be non-zero".into(),
            ));
        }
        if screen.available_width > screen.width {
            return Err(FingerprintApplyError::Inconsistent(
                "available_width must not exceed width".into(),
            ));
        }
        if screen.available_height > screen.height {
            return Err(FingerprintApplyError::Inconsistent(
                "available_height must not exceed height".into(),
            ));
        }
        let color_depth_ok = matches!(screen.color_depth, 24 | 30 | 32) || screen.color_depth >= 15;
        if !color_depth_ok {
            return Err(FingerprintApplyError::Inconsistent(
                "color_depth must be 24, 30, 32, or at least 15".into(),
            ));
        }
        if screen.pixel_ratio <= 0.0 {
            return Err(FingerprintApplyError::Inconsistent(
                "pixel ratio must be positive".into(),
            ));
        }

        if !(1..=3).contains(&self.canvas_noise_amplitude) {
            return Err(FingerprintApplyError::Inconsistent(
                "canvas noise amplitude out of range".into(),
            ));
        }

        if !is_lowercase_hex64(&self.canvas_hash) {
            return Err(FingerprintApplyError::Inconsistent(
                "canvas_hash must be 64-char lowercase hex".into(),
            ));
        }
        if !is_lowercase_hex64(&self.audio_hash) {
            return Err(FingerprintApplyError::Inconsistent(
                "audio_hash must be 64-char lowercase hex".into(),
            ));
        }
        if !is_lowercase_hex64(&self.webgl.hash) {
            return Err(FingerprintApplyError::Inconsistent(
                "webgl.hash must be 64-char lowercase hex".into(),
            ));
        }

        if self.inject_chrome && !self.user_agent.contains("Chrome/") {
            return Err(FingerprintApplyError::Inconsistent(
                "inject_chrome requires Chrome user-agent".into(),
            ));
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    let seed = config.session_seed;
    let canvas_masker = CanvasMasker::new(config.canvas.clone());
    let canvas_hash = canvas_masker.generate_hash(StdRng::seed_from_u64(seed));
    let canvas_noise_amplitude =
        canvas_masker.noise_amplitude(StdRng::seed_from_u64(seed.wrapping_add(10)));

    let webgl = webgl::build_profile(&config.webgl, StdRng::seed_from_u64(seed.wrapping_add(1)));

    let audio_masker = AudioMasker::new(config.audio.clone());
    let audio_hash = audio_masker.generate_hash(StdRng::seed_from_u64(seed.wrapping_add(2)));
    let audio_noise_scale = audio_masker.noise_scale(StdRng::seed_from_u64(seed.wrapping_add(11)));

    let font_masker = FontMasker::new(config.fonts.clone());
    let font_list = font_masker.get_standard_fonts();

    let screen_masker = ScreenMasker::new(config.screen.clone());
    let screen_res = screen_masker.get_spoofed_resolution();
    let user_agent = generate_user_agent(config.chrome_major, &config.platform);
    let client_hints = ua_ch::build_client_hints(config.chrome_major, &config.platform);

    FingerprintSession {
        session_id: format!("fp_{seed}"),
        session_seed: seed,
        canvas_hash,
        canvas_noise_amplitude,
        webgl,
        audio_hash,
        audio_noise_scale,
        font_list,
        screen_resolution: screen_res,
        user_agent,
        platform: config.platform.clone(),
        locale: config.locale.clone(),
        timezone_id: config.timezone_id.clone(),
        hardware_concurrency: config.hardware_concurrency,
        device_memory: config.device_memory,
        max_touch_points: config.max_touch_points,
        inject_chrome: config.inject_chrome,
        client_hints,
    }
}

fn generate_user_agent(chrome_major: u32, platform: &str) -> String {
    let major = chrome_major.max(100);
    match platform {
        "MacIntel" => format!(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36"
        ),
        "Linux x86_64" => format!(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36"
        ),
        _ => format!(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36"
        ),
    }
}
