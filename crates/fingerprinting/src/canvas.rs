//! Canvas and WebGL hash masking.
//!
//! Generates consistent, random-but-realistic hashes for Canvas and
//! WebGL contexts that remain stable across page visits.

use rand::rngs::StdRng;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Configuration for canvas masking.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CanvasConfig {
    #[serde(default = "default_hash_seed")]
    pub hash_seed: u64,
    #[serde(default = "default_canvas_id")]
    pub canvas_id: String,
}

impl Default for CanvasConfig {
    fn default() -> Self {
        Self {
            hash_seed: default_hash_seed(),
            canvas_id: default_canvas_id(),
        }
    }
}

fn default_hash_seed() -> u64 {
    42
}

fn default_canvas_id() -> String {
    "automation-canvas".to_string()
}

impl CanvasConfig {
    pub fn with_hash_seed(mut self, seed: u64) -> Self {
        self.hash_seed = seed;
        self
    }

    pub fn with_canvas_id(mut self, id: String) -> Self {
        self.canvas_id = id;
        self
    }
}

/// Canvas masker that generates consistent hashes.
pub struct CanvasMasker {
    config: CanvasConfig,
}

impl CanvasMasker {
    pub fn new(config: CanvasConfig) -> Self {
        Self { config }
    }

    /// Generate a consistent canvas hash for fingerprinting.
    pub fn generate_hash(&self, mut rng: StdRng) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.config.canvas_id.as_bytes());
        hasher.update(self.config.hash_seed.to_le_bytes());

        // Add random but deterministic noise
        let noise: [u8; 32] = rng.random();
        hasher.update(&noise);

        hex::encode(hasher.finalize())
    }
}
