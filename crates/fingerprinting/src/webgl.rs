//! WebGL vendor/renderer profile generation.

use rand::rngs::StdRng;
use rand::RngExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

fn default_max_texture_size() -> u32 {
    16384
}

/// Configuration for WebGL parameter spoofing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebGlConfig {
    #[serde(default = "default_vendor")]
    pub vendor: String,
    #[serde(default = "default_renderer")]
    pub renderer: String,
    #[serde(default = "default_max_texture_size")]
    pub max_texture_size: u32,
}

impl Default for WebGlConfig {
    fn default() -> Self {
        Self {
            vendor: default_vendor(),
            renderer: default_renderer(),
            max_texture_size: default_max_texture_size(),
        }
    }
}

fn default_vendor() -> String {
    "Google Inc. (NVIDIA)".to_string()
}

fn default_renderer() -> String {
    "ANGLE (NVIDIA, NVIDIA GeForce GTX 1660 Super Direct3D11 vs_5_0 ps_5_0, D3D11)".to_string()
}

impl WebGlConfig {
    pub fn with_vendor(mut self, vendor: impl Into<String>) -> Self {
        self.vendor = vendor.into();
        self
    }

    pub fn with_renderer(mut self, renderer: impl Into<String>) -> Self {
        self.renderer = renderer.into();
        self
    }

    pub fn with_max_texture_size(mut self, max_texture_size: u32) -> Self {
        self.max_texture_size = max_texture_size;
        self
    }
}

/// WebGL profile values applied via `getParameter` overrides.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WebGlProfile {
    pub vendor: String,
    pub renderer: String,
    pub hash: String,
    pub max_texture_size: u32,
}

/// Builds a stable WebGL profile from config + session RNG.
pub fn build_profile(config: &WebGlConfig, mut rng: StdRng) -> WebGlProfile {
    let mut hasher = Sha256::new();
    hasher.update(config.vendor.as_bytes());
    hasher.update(config.renderer.as_bytes());
    let noise: [u8; 16] = rng.random();
    hasher.update(noise);
    WebGlProfile {
        vendor: config.vendor.clone(),
        renderer: config.renderer.clone(),
        hash: hex::encode(hasher.finalize()),
        max_texture_size: config.max_texture_size,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    #[test]
    fn build_profile_default_max_texture_size() {
        let profile = build_profile(&WebGlConfig::default(), StdRng::seed_from_u64(1));
        assert_eq!(profile.max_texture_size, 16384);
    }
}
