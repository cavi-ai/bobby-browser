//! Canvas fingerprint profile generation.

use rand::rngs::StdRng;
use rand::RngExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Configuration for canvas masking.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CanvasConfig {
    #[serde(default = "default_hash_seed")]
    pub hash_seed: u64,
    /// Opaque seed material for canvas noise; never used as a DOM id.
    #[serde(default = "default_noise_key")]
    pub noise_key: String,
}

impl Default for CanvasConfig {
    fn default() -> Self {
        Self {
            hash_seed: default_hash_seed(),
            noise_key: default_noise_key(),
        }
    }
}

fn default_hash_seed() -> u64 {
    42
}

fn default_noise_key() -> String {
    "fp-canvas".to_string()
}

impl CanvasConfig {
    pub fn with_hash_seed(mut self, seed: u64) -> Self {
        self.hash_seed = seed;
        self
    }

    pub fn with_noise_key(mut self, key: impl Into<String>) -> Self {
        self.noise_key = key.into();
        self
    }

    /// Backward-compatible alias for [`Self::with_noise_key`].
    pub fn with_canvas_id(self, id: String) -> Self {
        self.with_noise_key(id)
    }
}

/// Canvas masker that generates consistent hashes and noise seeds.
pub struct CanvasMasker {
    config: CanvasConfig,
}

impl CanvasMasker {
    pub fn new(config: CanvasConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &CanvasConfig {
        &self.config
    }

    /// Generate a consistent canvas hash for fingerprinting.
    pub fn generate_hash(&self, mut rng: StdRng) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.config.noise_key.as_bytes());
        hasher.update(self.config.hash_seed.to_le_bytes());
        let noise: [u8; 32] = rng.random();
        hasher.update(noise);
        hex::encode(hasher.finalize())
    }

    /// Deterministic byte used as canvas pixel noise amplitude (1..=3).
    pub fn noise_amplitude(&self, mut rng: StdRng) -> u8 {
        1 + (rng.random::<u8>() % 3)
    }
}
