//! Typing simulator with human-like patterns.
//!
//! Introduces variable delays between keystrokes, simulates mistype/correct
//! cycles that preserve final text, select-all clear, and paste bursts.

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::{clamp_probability, order_u64, SessionRandom};

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
    #[serde(default = "default_copy_paste_probability")]
    pub copy_paste_probability: f64,
    /// Pause after this many *words* (whitespace-delimited), not characters.
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

fn default_copy_paste_probability() -> f64 {
    0.03
}

fn default_pause_after_words() -> u32 {
    4
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

    pub fn sanitize(mut self) -> Self {
        let (min, max) = order_u64(self.min_delay_ms, self.max_delay_ms);
        self.min_delay_ms = min;
        self.max_delay_ms = max.max(min + 1);
        self.correction_probability = clamp_probability(self.correction_probability);
        self.copy_paste_probability = clamp_probability(self.copy_paste_probability);
        if self.pause_after_words == 0 {
            self.pause_after_words = 1;
        }
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
    /// Platform-agnostic select-all; executor maps to Ctrl/Cmd+A.
    SelectAll {
        delay_ms: u64,
    },
    Backspace {
        count: u32,
        delay_ms: u64,
    },
    /// Insert `text` as a paste/burst (executor must insert these characters,
    /// not a bare Ctrl+V against an empty clipboard).
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
        Self {
            config: config.sanitize(),
        }
    }

    pub fn with_config(mut self, config: TextConfig) -> Self {
        self.config = config.sanitize();
        self
    }

    /// Generate typing actions for a given text string.
    ///
    /// Guarantees the composed key/paste stream yields `text` after execution
    /// (mistypes are always corrected before continuing).
    pub fn generate_actions(&self, random: &mut SessionRandom, text: &str) -> Vec<TypingAction> {
        let mut actions = Vec::new();
        let chars: Vec<char> = text.chars().collect();
        if chars.is_empty() {
            return actions;
        }

        let mut i = 0;
        let mut words_since_pause = 0u32;

        while i < chars.len() {
            // Word-boundary pause (after whitespace, once enough words elapsed).
            if i > 0 && chars[i - 1].is_whitespace() && !chars[i].is_whitespace() {
                words_since_pause = words_since_pause.saturating_add(1);
                if words_since_pause >= self.config.pause_after_words {
                    actions.push(TypingAction::Pause {
                        duration_ms: random.next_f64(
                            self.config.word_pause_ms as f64 * 0.5,
                            self.config.word_pause_ms as f64 * 2.0,
                        ) as u64,
                    });
                    words_since_pause = 0;
                }
            }

            // Paste burst for a contiguous run of non-whitespace.
            if !chars[i].is_whitespace()
                && i + 3 < chars.len()
                && random.chance(self.config.copy_paste_probability)
            {
                let mut end = i + random.gen_usize(3, 10.min(chars.len() - i));
                while end < chars.len() && end > i && chars[end - 1].is_whitespace() {
                    end -= 1;
                }
                if end > i {
                    let paste_text: String = chars[i..end].iter().collect();
                    if !paste_text.is_empty() {
                        actions.push(TypingAction::CopyPaste {
                            text: paste_text,
                            delay_ms: random.next_f64(80.0, 220.0) as u64,
                        });
                        i = end;
                        continue;
                    }
                }
            }

            let ch = chars[i];

            // Mistype then correct: wrong key → backspace → correct key.
            if !ch.is_whitespace() && random.chance(self.config.correction_probability) {
                let wrong = nearby_typo(ch, random);
                if wrong != ch {
                    self.push_key(&mut actions, random, wrong);
                    actions.push(TypingAction::Pause {
                        duration_ms: random.next_f64(40.0, 120.0) as u64,
                    });
                    actions.push(TypingAction::Backspace {
                        count: 1,
                        delay_ms: random.next_f64(60.0, 160.0) as u64,
                    });
                }
            }

            self.push_key(&mut actions, random, ch);
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

        if clear_first {
            actions.push(TypingAction::Pause {
                duration_ms: random.next_f64(80.0, 180.0) as u64,
            });
            actions.push(TypingAction::SelectAll {
                delay_ms: random.next_f64(40.0, 100.0) as u64,
            });
            actions.push(TypingAction::Backspace {
                count: 1,
                delay_ms: random.next_f64(80.0, 200.0) as u64,
            });
            actions.push(TypingAction::Pause {
                duration_ms: random.next_f64(100.0, 250.0) as u64,
            });
        }

        actions.extend(self.generate_actions(random, value));
        actions
    }

    fn push_key(&self, actions: &mut Vec<TypingAction>, random: &mut SessionRandom, ch: char) {
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
    }
}

fn nearby_typo(ch: char, random: &mut SessionRandom) -> char {
    const ROWS: [&str; 4] = ["1234567890", "qwertyuiop", "asdfghjkl", "zxcvbnm"];
    let lower = ch.to_ascii_lowercase();
    for row in ROWS {
        if let Some(idx) = row.chars().position(|c| c == lower) {
            let mut candidates = Vec::with_capacity(2);
            if idx > 0 {
                if let Some(c) = row.chars().nth(idx - 1) {
                    candidates.push(c);
                }
            }
            if idx + 1 < row.len() {
                if let Some(c) = row.chars().nth(idx + 1) {
                    candidates.push(c);
                }
            }
            candidates.retain(|c| *c != lower);
            if let Some(&typo) = candidates
                .get(random.gen_usize(0, candidates.len().saturating_sub(1)))
                .or_else(|| candidates.first())
            {
                return if ch.is_ascii_uppercase() {
                    typo.to_ascii_uppercase()
                } else {
                    typo
                };
            }
        }
    }
    let fallback = ['a', 'e', 'i', 'o', 'u', 'n', 't'];
    let mut pick = fallback[random.gen_usize(0, fallback.len() - 1)];
    if pick.eq_ignore_ascii_case(&ch) {
        pick = 'x';
    }
    if ch.is_ascii_uppercase() {
        pick.to_ascii_uppercase()
    } else {
        pick
    }
}

/// Replay typing actions into a string buffer (for tests / validation).
///
/// `SelectAll` marks the buffer as selected; the next `Backspace` clears it.
pub fn compose_typed_text(actions: &[TypingAction]) -> String {
    let mut buf = String::new();
    let mut selected = false;
    for action in actions {
        match action {
            TypingAction::KeyDown { character, .. } => {
                if selected {
                    buf.clear();
                    selected = false;
                }
                if let Some(ch) = character.chars().next() {
                    buf.push(ch);
                }
            }
            TypingAction::KeyUp { .. } | TypingAction::Pause { .. } => {}
            TypingAction::SelectAll { .. } => {
                selected = !buf.is_empty();
            }
            TypingAction::Backspace { count, .. } => {
                if selected {
                    buf.clear();
                    selected = false;
                } else {
                    for _ in 0..*count {
                        buf.pop();
                    }
                }
            }
            TypingAction::CopyPaste { text, .. } => {
                if selected {
                    buf.clear();
                    selected = false;
                }
                buf.push_str(text);
            }
        }
    }
    buf
}

impl TypingAction {
    /// The scripted delay this action contributes, in milliseconds.
    ///
    /// `Backspace` multiplies by `count`: the executor issues that many key
    /// events, each waiting `delay_ms`. Must match what the executor waits,
    /// since the sum is reported as `Evidence::Humanization.synthesized_ms`.
    pub fn synthesized_ms(&self) -> u64 {
        match self {
            Self::KeyDown { delay_ms, .. }
            | Self::KeyUp { delay_ms, .. }
            | Self::SelectAll { delay_ms }
            | Self::CopyPaste { delay_ms, .. } => *delay_ms,
            Self::Backspace { count, delay_ms } => delay_ms.saturating_mul(u64::from(*count)),
            Self::Pause { duration_ms } => *duration_ms,
        }
    }
}

/// Total scripted delay across a typing plan.
pub fn synthesized_total_ms(actions: &[TypingAction]) -> u64 {
    actions
        .iter()
        .map(TypingAction::synthesized_ms)
        .fold(0u64, u64::saturating_add)
}
