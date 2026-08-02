//! End-to-end behavioral contracts to iterate against.
//!
//! These exercise generator → stream shape → score gates without a browser.
//! Companion BiDi wiring lives in `firefox-companion/tests/behavioral_e2e.rs`.
//!
//! Run:
//! ```text
//! cargo test -p behavioral-engine --test e2e -- --nocapture
//! make behavioral-e2e
//! ```

use behavioral_engine::{
    compose_typed_text, gates, human_config, robot_config, run_benchmark, BezierMouseSimulator,
    ScrollAction, ScrollSimulator, SessionRandom, TypingAction, TypingSimulator,
};

const SEEDS: &[u64] = &[1, 7, 11, 42, 99, 123, 777, 1337, 9999, 42_424];

fn print_failing_dims(seed: u64, overall: f64, dims: &[behavioral_engine::DimensionScore]) {
    eprintln!("seed={seed} overall={overall:.1}");
    for dim in dims {
        if dim.score < 50.0 {
            eprintln!("  weak [{:>5.1}] {} — {}", dim.score, dim.id, dim.detail);
        }
    }
}

#[test]
fn multi_seed_human_clears_overall_and_curvature_gates() {
    let config = human_config();
    let mut failures = Vec::new();
    for &seed in SEEDS {
        let report = run_benchmark("human-e2e", seed, &config);
        if report.overall < gates::HUMAN_OVERALL_MIN {
            print_failing_dims(seed, report.overall, &report.dimensions);
            failures.push(format!(
                "seed {seed}: overall {:.1} < {}",
                report.overall,
                gates::HUMAN_OVERALL_MIN
            ));
        }
        let curve = report
            .dimensions
            .iter()
            .find(|d| d.id == "mouse.curvature")
            .expect("curvature");
        if curve.score < gates::MOUSE_CURVATURE_MIN {
            failures.push(format!(
                "seed {seed}: curvature {:.1} < {}",
                curve.score,
                gates::MOUSE_CURVATURE_MIN
            ));
        }
        let stress = report
            .dimensions
            .iter()
            .find(|d| d.id == "typing.stress_integrity")
            .expect("stress");
        if stress.score < gates::TYPING_STRESS_MIN {
            failures.push(format!(
                "seed {seed}: stress {:.1} < {}",
                stress.score,
                gates::TYPING_STRESS_MIN
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "human multi-seed gate failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn multi_seed_robot_stays_below_discrimination_gate() {
    let config = robot_config();
    let mut failures = Vec::new();
    for &seed in SEEDS {
        let report = run_benchmark("robot-e2e", seed, &config);
        if report.overall > gates::ROBOT_OVERALL_MAX_MULTI {
            print_failing_dims(seed, report.overall, &report.dimensions);
            failures.push(format!(
                "seed {seed}: robot overall {:.1} > {}",
                report.overall,
                gates::ROBOT_OVERALL_MAX_MULTI
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "robot discrimination failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn multi_seed_human_beats_robot_by_margin() {
    let mut failures = Vec::new();
    for &seed in SEEDS {
        let human = run_benchmark("human", seed, &human_config());
        let robot = run_benchmark("robot", seed, &robot_config());
        let margin = human.overall - robot.overall;
        println!(
            "seed={seed:>5} human={:.1} robot={:.1} margin={:+.1}",
            human.overall, robot.overall, margin
        );
        if margin < 10.0 {
            failures.push(format!(
                "seed {seed}: margin {margin:.1} (human {:.1} vs robot {:.1})",
                human.overall, robot.overall
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "discrimination margin failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn mouse_path_stream_is_curved_timed_and_settles() {
    let sim = BezierMouseSimulator::new(human_config().mouse);
    let mut failures = Vec::new();
    for &seed in SEEDS {
        let mut random = SessionRandom::new(seed);
        let path = sim.generate_path(&mut random, 12.0, 34.0, 640.0, 420.0);
        let points = path.points.len();
        if points < 8 {
            failures.push(format!("seed {seed}: only {points} points"));
        }
        if path.duration_ms < 80 {
            failures.push(format!(
                "seed {seed}: duration {}ms too short",
                path.duration_ms
            ));
        }
        if path.hover_dwell_ms < 35 {
            failures.push(format!(
                "seed {seed}: hover dwell {}ms below min",
                path.hover_dwell_ms
            ));
        }
        // Non-linear: midpoints should leave the straight chord.
        let start = path.points.first().unwrap();
        let end = path.points.last().unwrap();
        let dx = end.x - start.x;
        let dy = end.y - start.y;
        let chord = (dx * dx + dy * dy).sqrt().max(1.0);
        let max_dev = path
            .points
            .iter()
            .map(|p| {
                let t = ((p.x - start.x) * dx + (p.y - start.y) * dy) / (chord * chord);
                let px = start.x + t * dx;
                let py = start.y + t * dy;
                ((p.x - px).powi(2) + (p.y - py).powi(2)).sqrt()
            })
            .fold(0.0_f64, f64::max);
        if max_dev < 2.0 {
            failures.push(format!(
                "seed {seed}: max chord deviation {max_dev:.2}px (looks linear)"
            ));
        }
        // Timestamps must be non-decreasing with some positive steps.
        let mut prev = 0u64;
        let mut positive_steps = 0usize;
        for point in &path.points {
            if point.timestamp_ms < prev {
                failures.push(format!("seed {seed}: timestamp went backwards"));
                break;
            }
            if point.timestamp_ms > prev {
                positive_steps += 1;
            }
            prev = point.timestamp_ms;
        }
        if positive_steps < 4 {
            failures.push(format!(
                "seed {seed}: only {positive_steps} positive time steps"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "mouse stream failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn typing_stream_preserves_text_and_emits_inter_key_delays() {
    let text = "hello world from bobby";
    let sim = TypingSimulator::new(human_config().typing);
    let mut failures = Vec::new();
    for &seed in SEEDS {
        let mut random = SessionRandom::new(seed);
        let actions = sim.generate_with_clear(&mut random, text, true);
        let composed = compose_typed_text(&actions);
        if composed != text {
            failures.push(format!(
                "seed {seed}: composed {composed:?} != target {text:?}"
            ));
        }
        let pause_ms: u64 = actions
            .iter()
            .map(|action| match action {
                TypingAction::KeyDown { delay_ms, .. }
                | TypingAction::KeyUp { delay_ms, .. }
                | TypingAction::SelectAll { delay_ms }
                | TypingAction::Backspace { delay_ms, .. }
                | TypingAction::CopyPaste { delay_ms, .. } => *delay_ms,
                TypingAction::Pause { duration_ms } => *duration_ms,
            })
            .sum();
        if pause_ms < 200 {
            failures.push(format!(
                "seed {seed}: total delay/pause {pause_ms}ms too low"
            ));
        }
        let has_select_all = actions
            .iter()
            .any(|action| matches!(action, TypingAction::SelectAll { .. }));
        if !has_select_all {
            failures.push(format!("seed {seed}: clear_first missing SelectAll"));
        }
    }
    assert!(
        failures.is_empty(),
        "typing stream failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn scroll_long_travel_chunks_without_stacked_trailing_pauses() {
    let sim = ScrollSimulator::new(human_config().scroll);
    let mut failures = Vec::new();
    for &seed in SEEDS {
        let mut random = SessionRandom::new(seed);
        let actions = sim.generate_to_position(&mut random, 5200.0, 0.0, 800.0);
        let scrolls = actions
            .iter()
            .filter(|action| {
                matches!(
                    action,
                    ScrollAction::Scroll { .. } | ScrollAction::Bounce { .. }
                )
            })
            .count();
        if scrolls < 2 {
            failures.push(format!(
                "seed {seed}: expected chunked scrolls, got {scrolls}"
            ));
        }
        // Trailing pause stack: more than one consecutive Pause at the end is a bug.
        let trailing_pauses = actions
            .iter()
            .rev()
            .take_while(|action| matches!(action, ScrollAction::Pause { .. }))
            .count();
        if trailing_pauses > 1 {
            failures.push(format!(
                "seed {seed}: stacked trailing pauses ({trailing_pauses})"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "scroll stream failures:\n{}",
        failures.join("\n")
    );
}
