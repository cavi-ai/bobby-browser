//! Offline behavioral benchmark for interaction biometrics.
//!
//! Scores generated interaction streams (mouse, typing, scroll) against the
//! heuristics bot detectors use: curvature, velocity variance, keystroke CV,
//! pause structure, text integrity. Deterministic given a seed; no network.

use serde::{Deserialize, Serialize};

use crate::mouse::{BezierMouseSimulator, MouseConfig, MousePath};
use crate::scrolling::{ScrollAction, ScrollConfig, ScrollSimulator};
use crate::typing::{compose_typed_text, TextConfig, TypingAction, TypingSimulator};
use crate::{BehavioralConfig, SessionRandom};

/// One scored dimension (0–100).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DimensionScore {
    pub id: String,
    pub name: String,
    pub category: ScoreCategory,
    /// 0 = robotic / broken, 100 = passes human-likeness heuristic.
    pub score: f64,
    pub weight: f64,
    pub detail: String,
}

/// High-level score categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScoreCategory {
    Mouse,
    Typing,
    Scroll,
    Integrity,
}

/// Full benchmark report for one generator profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BehavioralBenchmarkReport {
    pub profile: String,
    pub seed: u64,
    pub overall: f64,
    pub dimensions: Vec<DimensionScore>,
}

impl BehavioralBenchmarkReport {
    pub fn category_average(&self, category: ScoreCategory) -> f64 {
        let items: Vec<_> = self
            .dimensions
            .iter()
            .filter(|d| d.category == category)
            .collect();
        if items.is_empty() {
            return 0.0;
        }
        let weight: f64 = items.iter().map(|d| d.weight).sum();
        if weight <= 0.0 {
            return 0.0;
        }
        items.iter().map(|d| d.score * d.weight).sum::<f64>() / weight
    }
}

fn clamp_score(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    value.clamp(0.0, 100.0)
}

fn weighted_overall(dimensions: &[DimensionScore]) -> f64 {
    let weight: f64 = dimensions.iter().map(|d| d.weight).sum();
    if weight <= 0.0 {
        return 0.0;
    }
    clamp_score(dimensions.iter().map(|d| d.score * d.weight).sum::<f64>() / weight)
}

fn dim(
    id: &str,
    name: &str,
    category: ScoreCategory,
    score: f64,
    weight: f64,
    detail: impl Into<String>,
) -> DimensionScore {
    DimensionScore {
        id: id.into(),
        name: name.into(),
        category,
        score: clamp_score(score),
        weight,
        detail: detail.into(),
    }
}

fn path_segment_speeds(path: &MousePath) -> Vec<f64> {
    let mut speeds = Vec::new();
    for window in path.points.windows(2) {
        let a = &window[0];
        let b = &window[1];
        let dt = b.timestamp_ms.saturating_sub(a.timestamp_ms).max(1) as f64;
        let dx = b.x - a.x;
        let dy = b.y - a.y;
        let dist = (dx * dx + dy * dy).sqrt();
        speeds.push(dist / dt);
    }
    speeds
}

fn mean_std(values: &[f64]) -> (f64, f64) {
    if values.is_empty() {
        return (0.0, 0.0);
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    if values.len() == 1 {
        return (mean, 0.0);
    }
    let var = values
        .iter()
        .map(|v| {
            let d = v - mean;
            d * d
        })
        .sum::<f64>()
        / (values.len() - 1) as f64;
    (mean, var.sqrt())
}

fn coefficient_of_variation(values: &[f64]) -> f64 {
    let (mean, std) = mean_std(values);
    if mean.abs() < 1e-9 {
        return 0.0;
    }
    std / mean.abs()
}

fn path_curvature_ratio(path: &MousePath) -> f64 {
    if path.points.len() < 2 {
        return 0.0;
    }
    let first = path.points.first().unwrap();
    let last = path.points.last().unwrap();
    let chord = ((last.x - first.x).powi(2) + (last.y - first.y).powi(2))
        .sqrt()
        .max(1.0);
    let mut arc = 0.0;
    for window in path.points.windows(2) {
        let dx = window[1].x - window[0].x;
        let dy = window[1].y - window[0].y;
        arc += (dx * dx + dy * dy).sqrt();
    }
    arc / chord
}

fn score_mouse(config: &MouseConfig, random: &mut SessionRandom) -> Vec<DimensionScore> {
    let sim = BezierMouseSimulator::new(config.clone());
    let mut paths = Vec::new();
    for _ in 0..12 {
        let x0 = random.next_f64(0.0, 200.0);
        let y0 = random.next_f64(0.0, 200.0);
        let x1 = random.next_f64(400.0, 900.0);
        let y1 = random.next_f64(200.0, 700.0);
        paths.push(sim.generate_path(random, x0, y0, x1, y1));
    }
    paths.push(sim.generate_approach_path(random));

    let curvatures: Vec<f64> = paths.iter().map(path_curvature_ratio).collect();
    let mean_curve = mean_std(&curvatures).0;
    // Straight line ≈ 1.0; human bezier typically 1.05–1.4.
    let curve_score = if mean_curve < 1.01 {
        10.0
    } else if mean_curve < 1.03 {
        40.0
    } else if mean_curve < 1.08 {
        70.0
    } else if mean_curve <= 1.6 {
        95.0
    } else {
        60.0
    };

    let mut speed_cvs = Vec::new();
    let mut mono_ok = 0usize;
    let mut dwell_ok = 0usize;
    let mut land_jitter = 0usize;
    for path in &paths {
        let speeds = path_segment_speeds(path);
        speed_cvs.push(coefficient_of_variation(&speeds));
        let mono = path
            .points
            .windows(2)
            .all(|w| w[1].timestamp_ms >= w[0].timestamp_ms);
        if mono {
            mono_ok += 1;
        }
        if path.hover_dwell_ms > 0 {
            dwell_ok += 1;
        }
        if let (Some(first), Some(last)) = (path.points.first(), path.points.last()) {
            // Approach paths land near origin with jitter — any non-exact end on long moves counts.
            let moved = ((last.x - first.x).powi(2) + (last.y - first.y).powi(2)).sqrt();
            if moved > 50.0 {
                land_jitter += 1;
            }
        }
    }
    let mean_speed_cv = mean_std(&speed_cvs).0;
    // Constant velocity → CV≈0; human-ish segments usually CV > 0.15.
    let velocity_score = if mean_speed_cv < 0.05 {
        15.0
    } else if mean_speed_cv < 0.12 {
        45.0
    } else if mean_speed_cv < 0.25 {
        75.0
    } else if mean_speed_cv < 0.9 {
        95.0
    } else {
        70.0
    };

    let mono_score = 100.0 * mono_ok as f64 / paths.len() as f64;
    let dwell_score = 100.0 * dwell_ok as f64 / paths.len() as f64;
    let sample_lens: Vec<f64> = paths.iter().map(|p| p.points.len() as f64).collect();
    let (mean_samples, sample_std) = mean_std(&sample_lens);
    let sample_score = if mean_samples < 4.0 {
        20.0
    } else if sample_std < 0.5 {
        55.0 // fixed sample count across distances
    } else {
        90.0
    };

    vec![
        dim(
            "mouse.curvature",
            "Path curvature vs chord",
            ScoreCategory::Mouse,
            curve_score,
            1.2,
            format!("mean arc/chord={mean_curve:.3}"),
        ),
        dim(
            "mouse.velocity_cv",
            "Segment velocity variance",
            ScoreCategory::Mouse,
            velocity_score,
            1.4,
            format!("mean speed CV={mean_speed_cv:.3}"),
        ),
        dim(
            "mouse.timestamps",
            "Monotonic timestamps",
            ScoreCategory::Mouse,
            mono_score,
            1.0,
            format!("{mono_ok}/{} paths monotonic", paths.len()),
        ),
        dim(
            "mouse.hover_dwell",
            "Pre-click hover dwell",
            ScoreCategory::Mouse,
            dwell_score,
            0.8,
            format!("{dwell_ok}/{} paths with dwell>0", paths.len()),
        ),
        dim(
            "mouse.sample_scaling",
            "Sample count scales with distance",
            ScoreCategory::Mouse,
            sample_score,
            0.9,
            format!("mean samples={mean_samples:.1} std={sample_std:.2}"),
        ),
        dim(
            "mouse.travel_present",
            "Non-trivial travel present",
            ScoreCategory::Mouse,
            100.0 * land_jitter as f64 / paths.len().max(1) as f64,
            0.5,
            format!("{land_jitter}/{} paths traveled >50px", paths.len()),
        ),
    ]
}

fn key_delays(actions: &[TypingAction]) -> Vec<f64> {
    actions
        .iter()
        .filter_map(|a| match a {
            TypingAction::KeyDown { delay_ms, .. } => Some(*delay_ms as f64),
            _ => None,
        })
        .collect()
}

fn score_typing(config: &TextConfig, random: &mut SessionRandom) -> Vec<DimensionScore> {
    let sim = TypingSimulator::new(config.clone());
    let corpus = [
        "hello world",
        "the quick brown fox jumps",
        "order-42 confirmation",
        "Ada Lovelace",
        "password-ish value",
    ];
    let mut integrity_ok = 0usize;
    let mut all_delays = Vec::new();
    let mut correction_runs = 0usize;
    let mut pause_runs = 0usize;
    let mut clear_ok = 0usize;

    for text in corpus {
        let actions = sim.generate_actions(random, text);
        if compose_typed_text(&actions) == text {
            integrity_ok += 1;
        }
        all_delays.extend(key_delays(&actions));
        if actions
            .iter()
            .any(|a| matches!(a, TypingAction::Backspace { .. }))
        {
            correction_runs += 1;
        }
        if actions
            .iter()
            .any(|a| matches!(a, TypingAction::Pause { .. }))
        {
            pause_runs += 1;
        }
    }

    let clear_actions = sim.generate_with_clear(random, "next", true);
    if clear_actions
        .iter()
        .any(|a| matches!(a, TypingAction::SelectAll { .. }))
        && compose_typed_text(&clear_actions) == "next"
    {
        clear_ok = 1;
    }

    // Force high correction rate sample for integrity under stress.
    // Only heavily weight this when the profile enables corrections.
    let stress = TypingSimulator::new(
        TextConfig::default()
            .with_correction_probability(1.0)
            .with_copy_paste_probability(0.0),
    );
    let stress_text = "integrity-under-mistypes";
    let stress_actions = stress.generate_actions(random, stress_text);
    let stress_ok = compose_typed_text(&stress_actions) == stress_text;
    let stress_weight = if config.correction_probability > 0.0 {
        1.6
    } else {
        0.4
    };

    let delay_cv = coefficient_of_variation(&all_delays);
    let delay_score = if delay_cv < 0.05 {
        20.0
    } else if delay_cv < 0.15 {
        55.0
    } else if delay_cv < 0.55 {
        95.0
    } else {
        75.0
    };

    let integrity_score = 100.0 * integrity_ok as f64 / corpus.len() as f64;
    let stress_score = if stress_ok { 100.0 } else { 0.0 };
    let correction_score = if correction_runs == 0 {
        // With default 8% it may miss on short corpus — soft fail.
        60.0
    } else {
        90.0
    };
    let pause_score = 100.0 * pause_runs as f64 / corpus.len() as f64;
    let clear_score = if clear_ok == 1 { 100.0 } else { 0.0 };

    vec![
        dim(
            "typing.delay_cv",
            "Keystroke delay variance",
            ScoreCategory::Typing,
            delay_score,
            1.3,
            format!("key-down delay CV={delay_cv:.3}"),
        ),
        dim(
            "typing.integrity",
            "Composed text matches intent",
            ScoreCategory::Typing,
            integrity_score,
            1.5,
            format!("{integrity_ok}/{} corpus strings exact", corpus.len()),
        ),
        dim(
            "typing.stress_integrity",
            "Integrity at 100% mistype rate",
            ScoreCategory::Typing,
            stress_score,
            stress_weight,
            if stress_ok {
                "mistype→backspace→retype preserves text"
            } else {
                "FAILED: stress mistypes corrupted output"
            },
        ),
        dim(
            "typing.corrections_present",
            "Correction events appear",
            ScoreCategory::Typing,
            correction_score,
            0.7,
            format!("{correction_runs}/{} runs emitted backspace", corpus.len()),
        ),
        dim(
            "typing.word_pauses",
            "Word-boundary pauses",
            ScoreCategory::Typing,
            pause_score,
            0.8,
            format!("{pause_runs}/{} runs had pauses", corpus.len()),
        ),
        dim(
            "typing.clear_first",
            "Select-all clear then type",
            ScoreCategory::Typing,
            clear_score,
            1.0,
            if clear_ok == 1 {
                "SelectAll present and final text correct"
            } else {
                "clear-first path broken"
            },
        ),
    ]
}

fn score_scroll(config: &ScrollConfig, random: &mut SessionRandom) -> Vec<DimensionScore> {
    let sim = ScrollSimulator::new(config.clone());
    let mut durations = Vec::new();
    let mut pause_count = 0usize;
    let mut bounce_count = 0usize;
    let mut samples = 0usize;

    for _ in 0..20 {
        let delta = random.gen_u32(120, 800) as i64;
        let actions = sim.generate_actions(random, delta, 1080.0);
        samples += 1;
        for action in &actions {
            match action {
                ScrollAction::Scroll { duration_ms, .. } => durations.push(*duration_ms as f64),
                ScrollAction::Bounce { duration_ms, .. } => {
                    bounce_count += 1;
                    durations.push(*duration_ms as f64);
                }
                ScrollAction::Pause { .. } => pause_count += 1,
            }
        }
    }

    let long = sim.generate_to_position(random, 5000.0, 0.0, 900.0);
    let chunked = long
        .iter()
        .filter(|a| matches!(a, ScrollAction::Scroll { .. }))
        .count();

    let dur_cv = coefficient_of_variation(&durations);
    let duration_score = if dur_cv < 0.05 {
        25.0
    } else if dur_cv < 0.2 {
        60.0
    } else {
        92.0
    };
    let pause_score = if pause_count == 0 {
        20.0
    } else if pause_count < samples {
        55.0
    } else {
        90.0
    };
    let bounce_score = if bounce_count == 0 { 50.0 } else { 85.0 };
    let chunk_score = if chunked >= 3 { 95.0 } else { 40.0 };

    vec![
        dim(
            "scroll.duration_cv",
            "Scroll duration variance",
            ScoreCategory::Scroll,
            duration_score,
            1.1,
            format!("duration CV={dur_cv:.3}"),
        ),
        dim(
            "scroll.pauses",
            "Reading / settle pauses",
            ScoreCategory::Scroll,
            pause_score,
            1.0,
            format!("{pause_count} pauses across {samples} scrolls"),
        ),
        dim(
            "scroll.bounce",
            "Occasional bounce-back",
            ScoreCategory::Scroll,
            bounce_score,
            0.6,
            format!("{bounce_count} bounce events in {samples} scrolls"),
        ),
        dim(
            "scroll.chunking",
            "Long distance chunking",
            ScoreCategory::Scroll,
            chunk_score,
            1.0,
            format!("{chunked} scroll chunks for 5000px travel"),
        ),
    ]
}

fn score_integrity(config: &BehavioralConfig, random: &mut SessionRandom) -> Vec<DimensionScore> {
    let sanitized = config.clone().sanitize();
    let order_ok = sanitized.mouse.min_duration_ms <= sanitized.mouse.max_duration_ms
        && sanitized.typing.min_delay_ms <= sanitized.typing.max_delay_ms
        && sanitized.scroll.min_scroll_duration_ms <= sanitized.scroll.max_scroll_duration_ms;

    let pause = crate::session_pause(random, &sanitized);
    let pause_score = if sanitized.session_jitter.is_zero() {
        50.0
    } else if pause.is_zero() {
        20.0
    } else if pause < sanitized.session_jitter {
        95.0
    } else {
        70.0
    };

    // Two seeds must not produce identical paths.
    let mut r2 = SessionRandom::new(random.seed().wrapping_add(99));
    let sim = BezierMouseSimulator::new(sanitized.mouse.clone());
    let a = sim.generate_path(random, 0.0, 0.0, 500.0, 300.0);
    let b = sim.generate_path(&mut r2, 0.0, 0.0, 500.0, 300.0);
    let diverse = a.points.len() != b.points.len()
        || a.duration_ms != b.duration_ms
        || a.points
            .iter()
            .zip(b.points.iter())
            .any(|(p, q)| (p.x - q.x).abs() > 1.0 || (p.y - q.y).abs() > 1.0);

    vec![
        dim(
            "integrity.config_sanitize",
            "Config sanitize orders ranges",
            ScoreCategory::Integrity,
            if order_ok { 100.0 } else { 0.0 },
            1.0,
            if order_ok {
                "min<=max for mouse/typing/scroll"
            } else {
                "range ordering broken after sanitize"
            },
        ),
        dim(
            "integrity.session_pause",
            "Session jitter produces pause",
            ScoreCategory::Integrity,
            pause_score,
            0.8,
            format!("pause={:?} jitter={:?}", pause, sanitized.session_jitter),
        ),
        dim(
            "integrity.seed_diversity",
            "Different seeds diverge paths",
            ScoreCategory::Integrity,
            if diverse { 100.0 } else { 15.0 },
            1.0,
            if diverse {
                "paths differ across seeds"
            } else {
                "paths identical across seeds"
            },
        ),
    ]
}

/// Run the full offline behavioral benchmark for a profile.
pub fn run_benchmark(
    profile: &str,
    seed: u64,
    config: &BehavioralConfig,
) -> BehavioralBenchmarkReport {
    let config = config.clone().sanitize();
    let mut random = SessionRandom::new(seed);
    let mut dimensions = Vec::new();
    dimensions.extend(score_mouse(&config.mouse, &mut random));
    dimensions.extend(score_typing(&config.typing, &mut random));
    dimensions.extend(score_scroll(&config.scroll, &mut random));
    dimensions.extend(score_integrity(&config, &mut random));
    let overall = weighted_overall(&dimensions);
    BehavioralBenchmarkReport {
        profile: profile.into(),
        seed,
        overall,
        dimensions,
    }
}

/// Intentionally robotic profile for a low-score baseline (straight timing, no noise).
pub fn robot_config() -> BehavioralConfig {
    BehavioralConfig::default()
        .with_mouse(
            MouseConfig::default()
                .with_min_duration(100)
                .with_max_duration(101)
                .with_control_variance(0.0)
                .with_curve_samples(3)
                .with_acceleration(1.0)
                .with_overshoot_probability(0.0)
                .with_landing_jitter(0.0)
                .with_path_noise(0.0)
                .with_hover_dwell_range(0, 0),
        )
        .with_typing(
            TextConfig::default()
                .with_min_delay(50)
                .with_max_delay(51)
                .with_correction_probability(0.0)
                .with_copy_paste_probability(0.0)
                .with_pause_after_words(1000)
                .with_word_pause(0),
        )
        .with_scroll(
            ScrollConfig::default()
                .with_min_duration(200)
                .with_max_duration(201)
                .with_read_pause_probability(0.0)
                .with_fast_scroll_probability(0.0)
                .with_bounce_probability(0.0)
                .with_trailing_read_pause(false),
        )
        .with_session_jitter(std::time::Duration::from_millis(0))
}

/// Human-oriented default profile under test.
pub fn human_config() -> BehavioralConfig {
    BehavioralConfig::default().sanitize()
}

/// Gate thresholds used by CI / `make behavioral-benchmark`.
pub mod gates {
    /// Default (human) profile must clear this overall score.
    pub const HUMAN_OVERALL_MIN: f64 = 70.0;
    /// Robot baseline should stay below this (proves the scorer discriminates).
    pub const ROBOT_OVERALL_MAX: f64 = 60.0;
    /// Multi-seed robot ceiling — allows ~1pt seed variance above the snapshot gate.
    pub const ROBOT_OVERALL_MAX_MULTI: f64 = 62.0;
    pub const TYPING_STRESS_MIN: f64 = 100.0;
    pub const MOUSE_CURVATURE_MIN: f64 = 40.0;
}
