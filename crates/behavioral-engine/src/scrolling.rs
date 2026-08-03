//! Scrolling simulator with human-like patterns.
//!
//! Adds random pauses, variable scroll speeds, and "scrolling to read"
//! behaviors (stopping mid-scroll) to mimic human browsing.

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::{clamp_probability, order_u64, SessionRandom};

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
    /// Always append a post-scroll settle/read pause.
    #[serde(default = "default_trailing_read_pause")]
    pub trailing_read_pause: bool,
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
            trailing_read_pause: default_trailing_read_pause(),
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

fn default_trailing_read_pause() -> bool {
    true
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

    pub fn with_trailing_read_pause(mut self, enabled: bool) -> Self {
        self.trailing_read_pause = enabled;
        self
    }

    pub fn sanitize(mut self) -> Self {
        let (min, max) = order_u64(self.min_scroll_duration_ms, self.max_scroll_duration_ms);
        self.min_scroll_duration_ms = min;
        self.max_scroll_duration_ms = max.max(min + 1);
        let (pmin, pmax) = order_u64(self.read_pause_min_ms, self.read_pause_max_ms);
        self.read_pause_min_ms = pmin;
        self.read_pause_max_ms = pmax.max(pmin + 1);
        self.read_pause_probability = clamp_probability(self.read_pause_probability);
        self.fast_scroll_probability = clamp_probability(self.fast_scroll_probability);
        self.bounce_probability = clamp_probability(self.bounce_probability);
        self
    }
}

/// A scrolling action for the behavioral engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScrollAction {
    Scroll { delta_y: i64, duration_ms: u64 },
    Pause { duration_ms: u64 },
    Bounce { delta_y: i64, duration_ms: u64 },
}

/// Scrolling simulator that generates human-like scroll patterns.
pub struct ScrollSimulator {
    config: ScrollConfig,
}

impl ScrollSimulator {
    pub fn new(config: ScrollConfig) -> Self {
        Self {
            config: config.sanitize(),
        }
    }

    pub fn with_config(mut self, config: ScrollConfig) -> Self {
        self.config = config.sanitize();
        self
    }

    /// Generate scroll actions for a scroll operation.
    pub fn generate_actions(
        &self,
        random: &mut SessionRandom,
        delta_y: i64,
        page_height: f64,
    ) -> Vec<ScrollAction> {
        self.generate_actions_inner(
            random,
            delta_y,
            page_height,
            self.config.trailing_read_pause,
        )
    }

    fn generate_actions_inner(
        &self,
        random: &mut SessionRandom,
        delta_y: i64,
        page_height: f64,
        include_trailing_read: bool,
    ) -> Vec<ScrollAction> {
        if delta_y == 0 {
            return Vec::new();
        }

        let mut actions = Vec::new();
        let is_fast_scroll = random.chance(self.config.fast_scroll_probability);
        let duration = if is_fast_scroll {
            random.next_duration(Duration::from_millis(50), Duration::from_millis(200))
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

        if random.chance(self.config.read_pause_probability) {
            let pause_ms = random.next_f64(
                self.config.read_pause_min_ms as f64,
                self.config.read_pause_max_ms as f64,
            );
            actions.push(ScrollAction::Pause {
                duration_ms: pause_ms as u64,
            });
        }

        if random.chance(self.config.bounce_probability) {
            let bounce = (delta_y as f64 * -0.1).round() as i64;
            if bounce != 0 {
                actions.push(ScrollAction::Bounce {
                    delta_y: bounce,
                    duration_ms: random.next_f64(100.0, 300.0) as u64,
                });
            }
        }

        if include_trailing_read {
            let read_ms = (page_height.max(0.0) * 10.0) as u64;
            let lo = (read_ms as f64 * 0.5).max(1.0);
            let hi = (read_ms as f64 * 2.0).max(lo + 1.0);
            actions.push(ScrollAction::Pause {
                duration_ms: random.next_f64(lo, hi) as u64,
            });
        }

        actions
    }

    /// Generate actions for scrolling to a specific position.
    ///
    /// Intermediate chunks omit the long trailing read pause; one settle pause
    /// is appended at the end.
    pub fn generate_to_position(
        &self,
        random: &mut SessionRandom,
        target_y: f64,
        current_y: f64,
        viewport_height: f64,
    ) -> Vec<ScrollAction> {
        let mut actions = Vec::new();
        let delta = (target_y - current_y) as i64;
        if delta == 0 {
            return actions;
        }

        let viewport_height = viewport_height.max(1.0);
        let chunk_size = (viewport_height * 0.8) as i64;
        let abs_delta = delta.abs();
        let sign = delta.signum();
        const MAX_CHUNKS: usize = 64;

        if abs_delta > chunk_size && chunk_size > 0 {
            let mut remaining = abs_delta;
            let mut chunks = 0usize;
            while remaining > 0 && chunks < MAX_CHUNKS {
                let chunk = if chunks + 1 == MAX_CHUNKS {
                    remaining
                } else {
                    chunk_size.min(remaining)
                };
                actions.extend(self.generate_actions_inner(
                    random,
                    chunk * sign,
                    viewport_height,
                    false,
                ));
                remaining -= chunk;
                chunks += 1;
                if remaining > 0 {
                    actions.push(ScrollAction::Pause {
                        duration_ms: random.next_f64(80.0, 280.0) as u64,
                    });
                }
            }
        } else {
            actions.extend(self.generate_actions_inner(random, delta, viewport_height, false));
        }

        // Exactly one settle pause at the end: drop trailing pauses so settle
        // never stacks on a chunk's probabilistic read pause.
        while matches!(actions.last(), Some(ScrollAction::Pause { .. })) {
            actions.pop();
        }

        let settle = (viewport_height * 8.0) as u64;
        let lo = (settle as f64 * 0.4).max(40.0);
        let hi = (settle as f64 * 1.2).max(lo + 1.0);
        actions.push(ScrollAction::Pause {
            duration_ms: random.next_f64(lo, hi) as u64,
        });

        actions
    }
}
