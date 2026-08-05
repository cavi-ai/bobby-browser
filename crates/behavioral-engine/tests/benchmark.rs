//! Offline behavioral biometric benchmark (CreepJS analogue for interactions).
//!
//! Run: `cargo test -p behavioral-engine --test benchmark -- --nocapture`
//! Or:  `make behavioral-benchmark`

use behavioral_engine::{gates, human_config, robot_config, run_benchmark, ScoreCategory};

fn print_report(label: &str, report: &behavioral_engine::BehavioralBenchmarkReport) {
    println!("=== {label} ===");
    println!(
        "profile={} seed={} overall={:.1}",
        report.profile, report.seed, report.overall
    );
    for category in [
        ScoreCategory::Mouse,
        ScoreCategory::Typing,
        ScoreCategory::Scroll,
        ScoreCategory::Integrity,
    ] {
        println!("  {:?}: {:.1}", category, report.category_average(category));
    }
    for dim in &report.dimensions {
        println!("  [{:>5.1}] {:<28} {}", dim.score, dim.id, dim.detail);
    }
    println!();
}

#[test]
fn human_profile_clears_overall_gate() {
    let report = run_benchmark("human-default", 42, &human_config());
    print_report("human-default", &report);
    assert!(
        report.overall >= gates::HUMAN_OVERALL_MIN,
        "human overall {:.1} < gate {}",
        report.overall,
        gates::HUMAN_OVERALL_MIN
    );
}

#[test]
fn robot_profile_stays_below_discrimination_gate() {
    let report = run_benchmark("robot-baseline", 42, &robot_config());
    print_report("robot-baseline", &report);
    assert!(
        report.overall <= gates::ROBOT_OVERALL_MAX,
        "robot overall {:.1} > gate {} (scorer not discriminating)",
        report.overall,
        gates::ROBOT_OVERALL_MAX
    );
}

#[test]
fn human_beats_robot_on_overall() {
    let human = run_benchmark("human-default", 7, &human_config());
    let robot = run_benchmark("robot-baseline", 7, &robot_config());
    print_report("human@7", &human);
    print_report("robot@7", &robot);
    assert!(
        human.overall > robot.overall + 10.0,
        "human {:.1} should beat robot {:.1} by >10",
        human.overall,
        robot.overall
    );
}

#[test]
fn typing_stress_integrity_is_perfect() {
    let report = run_benchmark("human-default", 99, &human_config());
    let stress = report
        .dimensions
        .iter()
        .find(|d| d.id == "typing.stress_integrity")
        .expect("stress dimension");
    assert!(
        stress.score >= gates::TYPING_STRESS_MIN,
        "stress integrity {:.1}",
        stress.score
    );
}

#[test]
fn mouse_curvature_not_linear() {
    let report = run_benchmark("human-default", 11, &human_config());
    let curve = report
        .dimensions
        .iter()
        .find(|d| d.id == "mouse.curvature")
        .expect("curvature dimension");
    assert!(
        curve.score >= gates::MOUSE_CURVATURE_MIN,
        "curvature {:.1} < {}",
        curve.score,
        gates::MOUSE_CURVATURE_MIN
    );
}

#[test]
fn benchmark_is_deterministic_for_seed() {
    let a = run_benchmark("human-default", 123, &human_config());
    let b = run_benchmark("human-default", 123, &human_config());
    assert_eq!(a.overall, b.overall);
    assert_eq!(a.dimensions.len(), b.dimensions.len());
    for (x, y) in a.dimensions.iter().zip(b.dimensions.iter()) {
        assert_eq!(x.id, y.id);
        assert_eq!(x.score, y.score);
    }
}

/// Review-pinned score table for the fixed seed. The gate assertions above
/// only prove the scorer prefers its own generators; this table turns any
/// scoring drift into a deliberate, reviewed edit. Regenerating it without
/// justification is exactly the failure mode it exists to catch -- say why
/// the score moved in the commit that moves it.
#[test]
fn seeded_scores_match_the_reviewed_expectation_table() {
    let fmt = |report: &behavioral_engine::BehavioralBenchmarkReport| {
        let categories = [
            ScoreCategory::Mouse,
            ScoreCategory::Typing,
            ScoreCategory::Scroll,
            ScoreCategory::Integrity,
        ]
        .map(|category| format!("{:.1}", report.category_average(category)))
        .join(",");
        let dimensions = report
            .dimensions
            .iter()
            .map(|dim| format!("{}={:.0}", dim.id, dim.score))
            .collect::<Vec<_>>()
            .join(",");
        format!("{:.1}|{categories}|{dimensions}", report.overall)
    };

    let human = run_benchmark("human-default", 42, &human_config());
    assert_eq!(
        fmt(&human),
        "93.0|91.0,93.4,91.1,98.6|mouse.curvature=70,mouse.velocity_cv=95,mouse.timestamps=100,mouse.hover_dwell=100,mouse.sample_scaling=90,mouse.travel_present=100,typing.delay_cv=95,typing.integrity=100,typing.stress_integrity=100,typing.corrections_present=90,typing.word_pauses=60,typing.clear_first=100,scroll.duration_cv=92,scroll.pauses=90,scroll.bounce=85,scroll.chunking=95,integrity.config_sanitize=100,integrity.session_pause=95,integrity.seed_diversity=100"
    );

    let robot = run_benchmark("robot-baseline", 42, &robot_config());
    assert_eq!(
        fmt(&robot),
        "58.1|47.3,62.8,46.6,85.7|mouse.curvature=10,mouse.velocity_cv=45,mouse.timestamps=100,mouse.hover_dwell=0,mouse.sample_scaling=55,mouse.travel_present=100,typing.delay_cv=20,typing.integrity=100,typing.stress_integrity=100,typing.corrections_present=60,typing.word_pauses=0,typing.clear_first=100,scroll.duration_cv=25,scroll.pauses=20,scroll.bounce=50,scroll.chunking=95,integrity.config_sanitize=100,integrity.session_pause=50,integrity.seed_diversity=100"
    );
}
