//! Behavioral engine for human-like browser automation.
//!
//! Provides mouse movement simulation (Bezier curves), typing simulation
//! (variable delays, corrections), and scrolling simulation (pauses,
//! variable speeds) to evade bot detection systems.

mod mouse;
mod score;
mod scrolling;
mod typing;

pub use mouse::{BezierMouseSimulator, MouseConfig, MousePath, MousePoint};
pub use score::{
    gates, human_config, robot_config, run_benchmark, BehavioralBenchmarkReport, DimensionScore,
    ScoreCategory,
};
pub use scrolling::{ScrollAction, ScrollConfig, ScrollSimulator};
pub use typing::{
    compose_typed_text, synthesized_total_ms, TextConfig, TypingAction, TypingSimulator,
};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Configuration for behavioral engine tuning.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BehavioralConfig {
    pub mouse: MouseConfig,
    pub typing: TextConfig,
    pub scroll: ScrollConfig,
    #[serde(default = "default_session_jitter")]
    pub session_jitter: Duration,
}

impl Default for BehavioralConfig {
    fn default() -> Self {
        Self {
            mouse: MouseConfig::default(),
            typing: TextConfig::default(),
            scroll: ScrollConfig::default(),
            session_jitter: default_session_jitter(),
        }
    }
}

fn default_session_jitter() -> Duration {
    Duration::from_millis(500)
}

impl BehavioralConfig {
    pub fn with_mouse(mut self, config: MouseConfig) -> Self {
        self.mouse = config;
        self
    }

    pub fn with_typing(mut self, config: TextConfig) -> Self {
        self.typing = config;
        self
    }

    pub fn with_scroll(mut self, config: ScrollConfig) -> Self {
        self.scroll = config;
        self
    }

    pub fn with_session_jitter(mut self, jitter: Duration) -> Self {
        self.session_jitter = jitter;
        self
    }

    /// Clamp probabilities and ensure min/max duration ordering.
    pub fn sanitize(mut self) -> Self {
        self.mouse = self.mouse.sanitize();
        self.typing = self.typing.sanitize();
        self.scroll = self.scroll.sanitize();
        self
    }
}

/// Sample a pre-interaction pause from session jitter.
pub fn session_pause(random: &mut SessionRandom, config: &BehavioralConfig) -> Duration {
    if config.session_jitter.is_zero() {
        return Duration::from_millis(0);
    }
    // Keep most pauses modest: 10%–60% of configured session jitter.
    let max = config.session_jitter;
    let min = max / 10;
    if min >= max {
        return max;
    }
    random.next_duration(min, max)
}

/// Session-level randomness generator for consistent-but-unique behavior.
#[derive(Clone)]
pub struct SessionRandom {
    seed: u64,
    rng: StdRng,
}

impl SessionRandom {
    pub fn new(seed: u64) -> Self {
        let rng = StdRng::seed_from_u64(seed);
        Self { seed, rng }
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    pub fn next_u64(&mut self) -> u64 {
        self.rng.random()
    }

    pub fn next_f64(&mut self, min: f64, max: f64) -> f64 {
        if !min.is_finite() || !max.is_finite() || min >= max {
            return min;
        }
        self.rng.random_range(min..max)
    }

    pub fn next_duration(&mut self, min: Duration, max: Duration) -> Duration {
        if min >= max {
            return min;
        }
        let min_ms = min.as_millis() as f64;
        let max_ms = max.as_millis() as f64;
        let ms = self.next_f64(min_ms, max_ms);
        Duration::from_millis(ms as u64)
    }

    pub fn jitter(&mut self, base: Duration) -> Duration {
        let inner = self.next_f64(0.5, 1.5);
        let jitter_range = self.next_f64(0.0, inner);
        let jitter_ms = base.as_millis() as f64 * jitter_range;
        base + Duration::from_millis(jitter_ms as u64)
    }

    /// Bernoulli trial with probability clamped to `[0, 1]`.
    pub fn chance(&mut self, probability: f64) -> bool {
        let p = probability.clamp(0.0, 1.0);
        if p <= 0.0 {
            return false;
        }
        if p >= 1.0 {
            return true;
        }
        self.rng.random_range(0.0..1.0) < p
    }

    pub fn gen_u32(&mut self, min: u32, max_inclusive: u32) -> u32 {
        if min >= max_inclusive {
            return min;
        }
        self.rng.random_range(min..=max_inclusive)
    }

    pub fn gen_usize(&mut self, min: usize, max_inclusive: usize) -> usize {
        if min >= max_inclusive {
            return min;
        }
        self.rng.random_range(min..=max_inclusive)
    }

    pub(crate) fn offset(&mut self, max_offset: f64) -> f64 {
        if max_offset <= 0.0 || !max_offset.is_finite() {
            return 0.0;
        }
        self.rng.random_range(-max_offset..max_offset)
    }
}

/// Generates a session seed from OS entropy, with a time-based fallback.
pub fn generate_session_seed() -> u64 {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_ok() {
        return u64::from_le_bytes(bytes);
    }
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let hash = (now as u64).wrapping_mul(2654435761);
    (hash ^ (now as u64 >> 32)).wrapping_add(42)
}

/// Human-like hesitation after a failed interaction.
pub fn hesitation_delay(random: &mut SessionRandom, failure_count: u32) -> Duration {
    let base_ms = 200.0 * (failure_count as f64).sqrt();
    random.next_duration(
        Duration::from_millis(base_ms as u64),
        Duration::from_millis((base_ms * 2.0).max(base_ms + 1.0) as u64),
    )
}

/// Simulates human "reading pause" after scrolling.
pub fn reading_pause(random: &mut SessionRandom, page_length: f64) -> Duration {
    let base_ms = page_length.max(0.0) * 15.0;
    random.next_duration(
        Duration::from_millis(base_ms as u64),
        Duration::from_millis((base_ms * 3.0).max(base_ms + 1.0) as u64),
    )
}

/// Simulates human "fast scroll" at end of page.
pub fn fast_scroll_pause(random: &mut SessionRandom) -> Duration {
    random.next_duration(Duration::from_millis(50), Duration::from_millis(200))
}

pub(crate) fn clamp_probability(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    value.clamp(0.0, 1.0)
}

pub(crate) fn order_u64(min: u64, max: u64) -> (u64, u64) {
    if min <= max {
        (min, max)
    } else {
        (max, min)
    }
}
