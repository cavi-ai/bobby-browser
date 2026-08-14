//! Screen resolution spoofing.

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
    #[serde(default = "default_taskbar_inset")]
    pub taskbar_inset: u32,
    /// Explicit browser-window size. When unset the masker computes a
    /// realistic non-maximized window (screen minus borders).
    #[serde(default)]
    pub window_width: Option<u32>,
    #[serde(default)]
    pub window_height: Option<u32>,
}

impl Default for ScreenConfig {
    fn default() -> Self {
        Self {
            width: default_width(),
            height: default_height(),
            color_depth: default_color_depth(),
            pixel_ratio: default_pixel_ratio(),
            taskbar_inset: default_taskbar_inset(),
            window_width: None,
            window_height: None,
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

fn default_taskbar_inset() -> u32 {
    40
}

impl ScreenConfig {
    pub fn with_width(mut self, width: u32) -> Self {
        self.width = width;
        self
    }

    pub fn with_height(mut self, height: u32) -> Self {
        self.height = height;
        self
    }

    pub fn with_color_depth(mut self, depth: u32) -> Self {
        self.color_depth = depth;
        self
    }

    pub fn with_pixel_ratio(mut self, ratio: f64) -> Self {
        self.pixel_ratio = ratio;
        self
    }

    pub fn with_taskbar_inset(mut self, inset: u32) -> Self {
        self.taskbar_inset = inset;
        self
    }
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
        let available_height = self.config.height.saturating_sub(self.config.taskbar_inset);
        // A real desktop window does not fill the screen: default to a
        // non-maximized window slightly smaller than the available area so
        // `innerWidth != screen.width` (CreepJS hasVvpScreenRes stays false).
        let window_width = self
            .config
            .window_width
            .unwrap_or_else(|| self.config.width.saturating_sub(20));
        let window_height = self
            .config
            .window_height
            .unwrap_or_else(|| available_height.saturating_sub(60));

        ScreenResolution {
            width: self.config.width,
            height: self.config.height,
            available_width: self.config.width,
            available_height,
            color_depth: self.config.color_depth,
            pixel_ratio: self.config.pixel_ratio,
            window_width: Some(window_width),
            window_height: Some(window_height),
        }
    }
}
