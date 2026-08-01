//! Screen resolution spoofing.
//!
//! Reports a standard resolution (e.g., 1920x1080) regardless of
//! actual hardware to prevent fingerprinting via screen detection.

use serde::{Deserialize, Serialize};

use crate::ScreenResolution;

/// Configuration for screen spoofing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScreenConfig {
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default = "default_color_depth")]
    pub color_depth: u32,
    #[serde(default = "default_pixel_ratio")]
    pub pixel_ratio: f64,
}

impl Default for ScreenConfig {
    fn default() -> Self {
        Self {
            width: default_width(),
            height: default_height(),
            color_depth: default_color_depth(),
            pixel_ratio: default_pixel_ratio(),
        }
    }
}

fn default_width() -> u32 {
    1920
}

fn default_height() -> u32 {
    1080
}

fn default_color_depth() -> u32 {
    24
}

fn default_pixel_ratio() -> f64 {
    1.0
}

/// Screen masker that returns spoofed resolution data.
pub struct ScreenMasker {
    config: ScreenConfig,
}

impl ScreenMasker {
    pub fn new(config: ScreenConfig) -> Self {
        Self { config }
    }

    /// Get the spoofed screen resolution.
    pub fn get_spoofed_resolution(&self) -> ScreenResolution {
        let available_height = self.config.height.saturating_sub(40); // Taskbar

        ScreenResolution {
            width: self.config.width,
            height: self.config.height,
            available_width: self.config.width,
            available_height,
            color_depth: self.config.color_depth,
            pixel_ratio: self.config.pixel_ratio,
        }
    }
}
