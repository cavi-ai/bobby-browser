//! Scrolling simulator with human-like patterns.
//!
//! Adds random pauses, variable scroll speeds, and "scrolling to read"
//! behaviors (stopping mid-scroll) to mimic human browsing.

use rand::Rng;

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::SessionRandom;

/// Configuration for scrolling simulation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScrollConfig {
    #[serde(default = "default_scroll_min_ms")]
    pub min_scroll_duration_ms: u64,
    #[serde(default = "default_scroll_max_ms")]
    pub max_scroll_duration_ms: u64,
    #[serde(default = "default_read_pause_probability")]
    pub read_pause_probability: f64,
    #[serde(default = "default_read_pause_min_ms")]
    pub read_pause_min_ms: u64,
    #[serde(default = "default_read_pause_max_ms")]
    pub read_pause_max_ms: u64,
    #[serde(default = "default_fast_scroll_probability")]
    pub fast_scroll_probability: f64,
    #[serde(default = "default_bounce_probability")]
    pub bounce_probability: f64,
}

impl Default for ScrollConfig {
    fn default() -> Self {
        Self {
            min_scroll_duration_ms: default_scroll_min_ms(),
            max_scroll_duration_ms: default_scroll_max_ms(),
            read_pause_probability: default_read_pause_probability(),
            read_pause_min_ms: default_read_pause_min_ms(),
            read_pause_max_ms: default_read_pause_max_ms(),
            fast_scroll_probability: default_fast_scroll_probability(),
            bounce_probability: default_bounce_probability(),
        }
    }
}

fn default_scroll_min_ms() -> u64 {
    200
}

fn default_scroll_max_ms() -> u64 {
    1500
}

fn default_read_pause_probability() -> f64 {
    0.3
}

fn default_read_pause_min_ms() -> u64 {
    500
}

fn default_read_pause_max_ms() -> u64 {
    3000
}

fn default_fast_scroll_probability() -> f64 {
    0.15
}

fn default_bounce_probability() -> f64 {
    0.05
}

impl ScrollConfig {
    pub fn with_min_duration(mut self, ms: u64) -> Self {
        self.min_scroll_duration_ms = ms;
        self
    }

    pub fn with_max_duration(mut self, ms: u64) -> Self {
        self.max_scroll_duration_ms = ms;
        self
    }

    pub fn with_read_pause_probability(mut self, prob: f64) -> Self {
        self.read_pause_probability = prob;
        self
    }

    pub fn with_read_pause_range(mut self, min_ms: u64, max_ms: u64) -> Self {
        self.read_pause_min_ms = min_ms;
        self.read_pause_max_ms = max_ms;
        self
    }

    pub fn with_fast_scroll_probability(mut self, prob: f64) -> Self {
        self.fast_scroll_probability = prob;
        self
    }

    pub fn with_bounce_probability(mut self, prob: f64) -> Self {
        self.bounce_probability = prob;
        self
    }
}

/// A scrolling action for the behavioral engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScrollAction {
    Scroll {
        delta_y: i64,
        duration_ms: u64,
    },
    Pause {
        duration_ms: u64,
    },
    Bounce {
        delta_y: i64,
        duration_ms: u64,
    },
}

/// Scrolling simulator that generates human-like scroll patterns.
pub struct ScrollSimulator {
    config: ScrollConfig,
}

impl ScrollSimulator {
    pub fn new(config: ScrollConfig) -> Self {
        Self { config }
    }

    pub fn with_config(mut self, config: ScrollConfig) -> Self {
        self.config = config;
        self
    }

    /// Generate scroll actions for a scroll operation.
    pub fn generate_actions(
        &self,
        random: &mut SessionRandom,
        delta_y: i64,
        page_height: f64,
    ) -> Vec<ScrollAction> {
        if delta_y == 0 {
            return Vec::new();
        }

        let mut actions = Vec::new();
        // Decide scroll speed
        let is_fast_scroll = random.rng.random_range(0.0..1.0) < self.config.fast_scroll_probability;
        let duration = if is_fast_scroll {
            random.next_duration(
                Duration::from_millis(50),
                Duration::from_millis(200),
            )
        } else {
            random.next_duration(
                Duration::from_millis(self.config.min_scroll_duration_ms),
                Duration::from_millis(self.config.max_scroll_duration_ms),
            )
        };

        actions.push(ScrollAction::Scroll {
            delta_y,
            duration_ms: duration.as_millis() as u64,
        });

        // Add "reading pause" (stop mid-scroll to read content)
        if random.rng.random_range(0.0..1.0) < self.config.read_pause_probability {
            let pause_ms = random.next_f64(
                self.config.read_pause_min_ms as f64,
                self.config.read_pause_max_ms as f64,
            );
            actions.push(ScrollAction::Pause {
                duration_ms: pause_ms as u64,
            });
        }

        // Add bounce back (human tendency to scroll back slightly)
        if random.rng.random_range(0.0..1.0) < self.config.bounce_probability {
            let bounce = (delta_y as f64 * -0.1).round() as i64;
            if bounce != 0 {
                actions.push(ScrollAction::Bounce {
                    delta_y: bounce,
                    duration_ms: random.next_f64(100.0, 300.0) as u64,
                });
            }
        }

        // Add post-scroll reading pause based on page length
        let read_ms = (page_height * 10.0) as u64;
        actions.push(ScrollAction::Pause {
            duration_ms: random.next_f64(read_ms as f64 * 0.5, read_ms as f64 * 2.0) as u64,
        });

        actions
    }

    /// Generate actions for scrolling to a specific position.
    pub fn generate_to_position(
        &self,
        random: &mut SessionRandom,
        target_y: f64,
        current_y: f64,
        viewport_height: f64,
    ) -> Vec<ScrollAction> {
        let mut actions = Vec::new();
        let _rng = &mut random.rng;

        let delta = (target_y - current_y) as i64;
        if delta == 0 {
            return actions;
        }

        // Scroll in chunks for long distances
        let chunk_size = (viewport_height * 0.8) as i64;
        let _y = current_y as i64;
        let abs_delta = delta.abs();

        if abs_delta > chunk_size {
            let mut remaining = abs_delta;
            while remaining > 0 {
                let chunk = chunk_size.min(remaining);
                actions.extend(self.generate_actions(random, chunk * (delta.signum() as i64), viewport_height));
                remaining -= chunk;
                if remaining > 0 {
                    actions.push(ScrollAction::Pause {
                        duration_ms: random.next_f64(100.0, 400.0) as u64,
                    });
                }
            }
        } else {
            actions.extend(self.generate_actions(random, delta, viewport_height));
        }

        actions
    }
}
