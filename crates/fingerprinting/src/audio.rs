//! AudioContext fingerprint masking.

use rand::rngs::StdRng;
use rand::RngExt;
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

impl AudioConfig {
    pub fn with_hash_seed(mut self, seed: u64) -> Self {
        self.hash_seed = seed;
        self
    }

    pub fn with_analyser_size(mut self, size: usize) -> Self {
        self.analyser_size = size;
        self
    }
}

/// Audio masker that generates consistent noise patterns.
pub struct AudioMasker {
    config: AudioConfig,
}

impl AudioMasker {
    pub fn new(config: AudioConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &AudioConfig {
        &self.config
    }

    /// Generate a consistent audio hash for fingerprinting.
    pub fn generate_hash(&self, mut rng: StdRng) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.config.hash_seed.to_le_bytes());
        hasher.update(self.config.analyser_size.to_le_bytes());
        let noise: [u8; 64] = rng.random();
        hasher.update(noise);
        hex::encode(hasher.finalize())
    }

    /// Get the standard analyser buffer size.
    pub fn analyser_size(&self) -> usize {
        self.config.analyser_size
    }

    /// Deterministic float noise scale applied to AudioBuffer channel data.
    pub fn noise_scale(&self, mut rng: StdRng) -> f64 {
        1e-7 + (rng.random::<f64>() * 1e-7)
    }
}
