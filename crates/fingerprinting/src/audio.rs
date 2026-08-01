//! AudioContext fingerprint masking.
//!
//! Returns consistent noise for audio fingerprinting to avoid
//! detection while maintaining realistic audio context behavior.

use rand::rngs::StdRng;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Configuration for audio masking.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AudioConfig {
    #[serde(default = "default_audio_seed")]
    pub hash_seed: u64,
    #[serde(default = "default_analyser_size")]
    pub analyser_size: usize,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            hash_seed: default_audio_seed(),
            analyser_size: default_analyser_size(),
        }
    }
}

fn default_audio_seed() -> u64 {
    137
}

fn default_analyser_size() -> usize {
    2048
}

/// Audio masker that generates consistent noise patterns.
pub struct AudioMasker {
    config: AudioConfig,
}

impl AudioMasker {
    pub fn new(config: AudioConfig) -> Self {
        Self { config }
    }

    /// Generate a consistent audio hash for fingerprinting.
    pub fn generate_hash(&self, mut rng: StdRng) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.config.hash_seed.to_le_bytes());

        // Generate deterministic noise pattern
        let noise: [u8; 64] = rng.random();
        hasher.update(&noise);

        hex::encode(hasher.finalize())
    }

    /// Get the standard analyser buffer size.
    pub fn analyser_size(&self) -> usize {
        self.config.analyser_size
    }
}
