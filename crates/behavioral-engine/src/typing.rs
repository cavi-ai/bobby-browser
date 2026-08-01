//! Typing simulator with human-like patterns.
//!
//! Introduces variable delays between keystrokes, simulates backspaces,
//! corrections, and copy-paste actions to avoid detection.

use rand::Rng;

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::SessionRandom;

/// Configuration for typing simulation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TextConfig {
    #[serde(default = "default_key_min_ms")]
    pub min_delay_ms: u64,
    #[serde(default = "default_key_max_ms")]
    pub max_delay_ms: u64,
    #[serde(default = "default_correction_probability")]
    pub correction_probability: f64,
    #[serde(default = "default_backspace_count")]
    pub max_backspaces: u32,
    #[serde(default = "default_copy_paste_probability")]
    pub copy_paste_probability: f64,
    #[serde(default = "default_pause_after_words")]
    pub pause_after_words: u32,
    #[serde(default = "default_word_pause_ms")]
    pub word_pause_ms: u64,
}

impl Default for TextConfig {
    fn default() -> Self {
        Self {
            min_delay_ms: default_key_min_ms(),
            max_delay_ms: default_key_max_ms(),
            correction_probability: default_correction_probability(),
            max_backspaces: default_backspace_count(),
            copy_paste_probability: default_copy_paste_probability(),
            pause_after_words: default_pause_after_words(),
            word_pause_ms: default_word_pause_ms(),
        }
    }
}

fn default_key_min_ms() -> u64 {
    30
}

fn default_key_max_ms() -> u64 {
    150
}

fn default_correction_probability() -> f64 {
    0.08
}

fn default_backspace_count() -> u32 {
    3
}

fn default_copy_paste_probability() -> f64 {
    0.03
}

fn default_pause_after_words() -> u32 {
    8
}

fn default_word_pause_ms() -> u64 {
    200
}

impl TextConfig {
    pub fn with_min_delay(mut self, ms: u64) -> Self {
        self.min_delay_ms = ms;
        self
    }

    pub fn with_max_delay(mut self, ms: u64) -> Self {
        self.max_delay_ms = ms;
        self
    }

    pub fn with_correction_probability(mut self, prob: f64) -> Self {
        self.correction_probability = prob;
        self
    }

    pub fn with_max_backspaces(mut self, count: u32) -> Self {
        self.max_backspaces = count;
        self
    }

    pub fn with_copy_paste_probability(mut self, prob: f64) -> Self {
        self.copy_paste_probability = prob;
        self
    }

    pub fn with_pause_after_words(mut self, count: u32) -> Self {
        self.pause_after_words = count;
        self
    }

    pub fn with_word_pause(mut self, ms: u64) -> Self {
        self.word_pause_ms = ms;
        self
    }
}

/// A typing action for the behavioral engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TypingAction {
    KeyDown {
        character: String,
        delay_ms: u64,
    },
    KeyUp {
        character: String,
        delay_ms: u64,
    },
    Backspace {
        count: u32,
        delay_ms: u64,
    },
    CopyPaste {
        text: String,
        delay_ms: u64,
    },
    Pause {
        duration_ms: u64,
    },
}

/// Typing simulator that generates human-like typing patterns.
pub struct TypingSimulator {
    config: TextConfig,
}

impl TypingSimulator {
    pub fn new(config: TextConfig) -> Self {
        Self { config }
    }

    pub fn with_config(mut self, config: TextConfig) -> Self {
        self.config = config;
        self
    }

    /// Generate typing actions for a given text string.
    pub fn generate_actions(
        &self,
        random: &mut SessionRandom,
        text: &str,
    ) -> Vec<TypingAction> {
        let mut actions = Vec::new();
        let chars: Vec<char> = text.chars().collect();
        let mut word_char_count = 0u32;
        let mut i = 0;

        while i < chars.len() {
            let ch = chars[i];
            word_char_count += 1;

            // Add word-level pause
            if word_char_count >= self.config.pause_after_words && i < chars.len() - 1 {
                actions.push(TypingAction::Pause {
                    duration_ms: random.next_f64(
                        self.config.word_pause_ms as f64 * 0.5,
                        self.config.word_pause_ms as f64 * 2.0,
                    ) as u64,
                });
                word_char_count = 0;
            }

            // Decide whether to make a correction
            if random.rng.random_range(0.0..1.0) < self.config.correction_probability && i > 0 {
                let backspace_count = random.rng.random_range(1..=self.config.max_backspaces);
                actions.push(TypingAction::Backspace {
                    count: backspace_count,
                    delay_ms: random.next_f64(80.0, 200.0) as u64,
                });
            }

            // Decide whether to use copy-paste instead of typing
            if random.rng.random_range(0.0..1.0) < self.config.copy_paste_probability && i > 2 {
                let paste_len = random.rng.random_range(3..=10).min(chars.len() - i);
                let paste_text: String = chars[i..i + paste_len].iter().collect();
                actions.push(TypingAction::CopyPaste {
                    text: paste_text,
                    delay_ms: random.next_f64(100.0, 300.0) as u64,
                });
                i += paste_len;
                continue;
            }

            // Generate key down/up with variable delay
            let delay_ms = random.next_duration(
                Duration::from_millis(self.config.min_delay_ms),
                Duration::from_millis(self.config.max_delay_ms),
            );

            actions.push(TypingAction::KeyDown {
                character: ch.to_string(),
                delay_ms: delay_ms.as_millis() as u64,
            });

            actions.push(TypingAction::KeyUp {
                character: ch.to_string(),
                delay_ms: random.next_f64(10.0, 50.0) as u64,
            });

            i += 1;
        }

        actions
    }

    /// Generate actions for a typed value with optional clear-first behavior.
    pub fn generate_with_clear(
        &self,
        random: &mut SessionRandom,
        value: &str,
        clear_first: bool,
    ) -> Vec<TypingAction> {
        let mut actions = Vec::new();

        if clear_first && !value.is_empty() {
            // Simulate select-all + delete
            actions.push(TypingAction::Pause {
                duration_ms: random.next_f64(100.0, 200.0) as u64,
            });
            // Ctrl+A
            actions.push(TypingAction::Pause {
                duration_ms: random.next_f64(50.0, 100.0) as u64,
            });
            // Delete
            actions.push(TypingAction::Backspace {
                count: 1,
                delay_ms: random.next_f64(100.0, 250.0) as u64,
            });
            actions.push(TypingAction::Pause {
                duration_ms: random.next_f64(150.0, 300.0) as u64,
            });
        }

        actions.extend(self.generate_actions(random, value));

        actions
    }
}
