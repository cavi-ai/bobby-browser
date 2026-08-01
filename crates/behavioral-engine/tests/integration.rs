use behavioral_engine::{
    compose_typed_text, generate_session_seed, BezierMouseSimulator, BehavioralConfig, MouseConfig,
    ScrollConfig, ScrollSimulator, SessionRandom, TextConfig, TypingSimulator,
};

#[test]
fn generate_session_seed_produces_values() {
    let seed = generate_session_seed();
    assert_ne!(seed, 0);
}

#[test]
fn session_random_deterministic_by_seed() {
    let mut r1 = SessionRandom::new(42);
    let mut r2 = SessionRandom::new(42);

    for _ in 0..100 {
        assert_eq!(r1.next_u64(), r2.next_u64());
        assert!((r1.next_f64(0.0, 1.0) - r2.next_f64(0.0, 1.0)).abs() < 1e-10);
    }
}

#[test]
fn session_random_different_seeds_differ() {
    let mut r1 = SessionRandom::new(1);
    let mut r2 = SessionRandom::new(2);

    assert_ne!(r1.next_u64(), r2.next_u64());
}

#[test]
fn session_random_f64_range() {
    let mut r = SessionRandom::new(99);
    for _ in 0..50 {
        let val = r.next_f64(10.0, 20.0);
        assert!((10.0..20.0).contains(&val), "value {val} out of range [10, 20)");
    }
}

#[test]
fn session_random_duration() {
    let mut r = SessionRandom::new(77);
    let dur = r.next_duration(
        std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(200),
    );
    assert!(dur.as_millis() >= 50 && dur.as_millis() < 200);
}

#[test]
fn session_random_jitter() {
    let mut r = SessionRandom::new(33);
    let base = std::time::Duration::from_millis(100);
    let jittered = r.jitter(base);
    assert!(jittered.as_millis() >= base.as_millis());
}

#[test]
fn session_random_seed_preserved() {
    let r = SessionRandom::new(12345);
    assert_eq!(r.seed(), 12345);
}

#[test]
fn session_random_inverted_range_is_safe() {
    let mut r = SessionRandom::new(1);
    assert_eq!(r.next_f64(5.0, 5.0), 5.0);
    assert_eq!(
        r.next_duration(
            std::time::Duration::from_millis(10),
            std::time::Duration::from_millis(10)
        ),
        std::time::Duration::from_millis(10)
    );
}

#[test]
fn bezier_mouse_path_basic() {
    let sim = BezierMouseSimulator::new(MouseConfig::default());
    let mut random = SessionRandom::new(100);

    let path = sim.generate_path(&mut random, 0.0, 0.0, 500.0, 300.0);

    assert!(path.points.len() >= 5);
    assert!(path.duration_ms > 0);

    // First point should be exact start; last near target (landing jitter allowed).
    let first = path.points.first().unwrap();
    assert!((first.x - 0.0).abs() < 1e-9);
    assert!((first.y - 0.0).abs() < 1e-9);

    let last = path.points.last().unwrap();
    assert!((last.x - 500.0).abs() <= 24.0);
    assert!((last.y - 300.0).abs() <= 24.0);
}

#[test]
fn bezier_mouse_path_timestamps_increasing() {
    let sim = BezierMouseSimulator::new(MouseConfig::default());
    let mut random = SessionRandom::new(200);

    let path = sim.generate_path(&mut random, 100.0, 100.0, 400.0, 200.0);

    for i in 1..path.points.len() {
        assert!(
            path.points[i].timestamp_ms >= path.points[i - 1].timestamp_ms,
            "timestamps not monotonically increasing"
        );
    }
}

#[test]
fn bezier_mouse_path_short_distance() {
    let sim = BezierMouseSimulator::new(MouseConfig::default());
    let mut random = SessionRandom::new(300);

    let path = sim.generate_path(&mut random, 0.0, 0.0, 10.0, 10.0);

    assert!(path.points.len() >= 3);
}

#[test]
fn bezier_mouse_config_min_max_duration() {
    let config = MouseConfig::default()
        .with_min_duration(200)
        .with_max_duration(500);
    let sim = BezierMouseSimulator::new(config);
    let mut random = SessionRandom::new(400);

    let path = sim.generate_path(&mut random, 0.0, 0.0, 300.0, 200.0);

    assert!(path.duration_ms >= 200);
    assert!(path.duration_ms <= 500);
}

#[test]
fn bezier_approach_path_ends_near_origin() {
    let sim = BezierMouseSimulator::new(MouseConfig::default());
    let mut random = SessionRandom::new(401);
    let path = sim.generate_approach_path(&mut random);
    let last = path.points.last().unwrap();
    assert!(last.x.abs() <= 24.0, "landing x too far: {}", last.x);
    assert!(last.y.abs() <= 24.0, "landing y too far: {}", last.y);
    assert!(path.hover_dwell_ms > 0);
}

#[test]
fn bezier_rejects_non_finite_inputs() {
    let sim = BezierMouseSimulator::new(MouseConfig::default());
    let mut random = SessionRandom::new(402);
    let path = sim.generate_path(&mut random, f64::NAN, f64::INFINITY, 10.0, 10.0);
    assert!(path.points.iter().all(|p| p.x.is_finite() && p.y.is_finite()));
}

#[test]
fn typing_select_all_clears_prior_buffer_in_compose() {
    use behavioral_engine::TypingAction;
    let actions = vec![
        TypingAction::KeyDown {
            character: "a".into(),
            delay_ms: 1,
        },
        TypingAction::KeyUp {
            character: "a".into(),
            delay_ms: 1,
        },
        TypingAction::SelectAll { delay_ms: 1 },
        TypingAction::Backspace {
            count: 1,
            delay_ms: 1,
        },
        TypingAction::KeyDown {
            character: "z".into(),
            delay_ms: 1,
        },
        TypingAction::KeyUp {
            character: "z".into(),
            delay_ms: 1,
        },
    ];
    assert_eq!(compose_typed_text(&actions), "z");
}

#[test]
fn session_pause_respects_jitter_band() {
    use behavioral_engine::session_pause;
    let config = BehavioralConfig::default().with_session_jitter(std::time::Duration::from_millis(500));
    let mut random = SessionRandom::new(7);
    for _ in 0..20 {
        let pause = session_pause(&mut random, &config);
        assert!(pause.as_millis() >= 50);
        assert!(pause.as_millis() < 500);
    }
}

#[test]
fn typing_simulator_basic() {
    let sim = TypingSimulator::new(TextConfig::default());
    let mut random = SessionRandom::new(500);

    let actions = sim.generate_actions(&mut random, "hello");

    assert!(actions.len() >= 2);
    assert_eq!(compose_typed_text(&actions), "hello");
}

#[test]
fn typing_simulator_longer_text() {
    let sim = TypingSimulator::new(TextConfig::default());
    let mut random = SessionRandom::new(600);

    let text = "this is a longer piece of text for testing";
    let actions = sim.generate_actions(&mut random, text);

    assert!(actions.len() > 10);
    assert_eq!(compose_typed_text(&actions), text);
}

#[test]
fn typing_corrections_preserve_final_text() {
    let sim = TypingSimulator::new(
        TextConfig::default()
            .with_correction_probability(1.0)
            .with_copy_paste_probability(0.0),
    );
    let mut random = SessionRandom::new(601);
    let text = "correctness";
    let actions = sim.generate_actions(&mut random, text);
    assert_eq!(compose_typed_text(&actions), text);
    assert!(actions.iter().any(|a| matches!(
        a,
        behavioral_engine::TypingAction::Backspace { .. }
    )));
}

#[test]
fn typing_simulator_clear_first() {
    let sim = TypingSimulator::new(TextConfig::default());
    let mut random = SessionRandom::new(700);

    let actions = sim.generate_with_clear(&mut random, "value", true);

    assert!(actions.iter().any(|a| matches!(
        a,
        behavioral_engine::TypingAction::SelectAll { .. }
    )));
    assert!(actions.len() > 5);
    assert_eq!(compose_typed_text(&actions), "value");
}

#[test]
fn typing_simulator_empty_string() {
    let sim = TypingSimulator::new(TextConfig::default());
    let mut random = SessionRandom::new(800);

    let actions = sim.generate_actions(&mut random, "");
    assert!(actions.is_empty());
}

#[test]
fn typing_simulator_unicode() {
    let sim = TypingSimulator::new(
        TextConfig::default()
            .with_correction_probability(0.0)
            .with_copy_paste_probability(0.0),
    );
    let mut random = SessionRandom::new(900);

    let text = "héllo 世界";
    let actions = sim.generate_actions(&mut random, text);
    assert_eq!(compose_typed_text(&actions), text);
}

#[test]
fn scroll_simulator_basic() {
    let sim = ScrollSimulator::new(ScrollConfig::default());
    let mut random = SessionRandom::new(1000);

    let actions = sim.generate_actions(&mut random, 500, 1080.0);

    assert!(actions.len() >= 2);
}

#[test]
fn scroll_simulator_negative_delta() {
    let sim = ScrollSimulator::new(ScrollConfig::default());
    let mut random = SessionRandom::new(1100);

    let actions = sim.generate_actions(&mut random, -300, 1080.0);

    assert!(!actions.is_empty());
}

#[test]
fn scroll_simulator_zero_delta() {
    let sim = ScrollSimulator::new(ScrollConfig::default());
    let mut random = SessionRandom::new(1200);

    let actions = sim.generate_actions(&mut random, 0, 1080.0);

    assert!(actions.is_empty());
}

#[test]
fn scroll_simulator_to_position() {
    let sim = ScrollSimulator::new(ScrollConfig::default());
    let mut random = SessionRandom::new(1300);

    let actions = sim.generate_to_position(&mut random, 2000.0, 0.0, 1080.0);

    assert!(actions.len() >= 2);
}

#[test]
fn scroll_simulator_to_position_same() {
    let sim = ScrollSimulator::new(ScrollConfig::default());
    let mut random = SessionRandom::new(1400);

    let actions = sim.generate_to_position(&mut random, 100.0, 100.0, 1080.0);

    assert!(actions.is_empty());
}

#[test]
fn behavioral_config_default() {
    let config = BehavioralConfig::default();
    assert!(config.mouse.min_duration_ms > 0);
    assert!(config.typing.min_delay_ms > 0);
    assert!(config.scroll.min_scroll_duration_ms > 0);
}

#[test]
fn behavioral_config_chain() {
    let config = BehavioralConfig::default()
        .with_mouse(MouseConfig::default().with_min_duration(100))
        .with_typing(TextConfig::default().with_min_delay(50))
        .with_scroll(ScrollConfig::default().with_min_duration(300));

    assert_eq!(config.mouse.min_duration_ms, 100);
    assert_eq!(config.typing.min_delay_ms, 50);
    assert_eq!(config.scroll.min_scroll_duration_ms, 300);
}

#[test]
fn behavioral_config_sanitize_orders_ranges() {
    let config = BehavioralConfig::default()
        .with_mouse(
            MouseConfig::default()
                .with_min_duration(500)
                .with_max_duration(100),
        )
        .sanitize();
    assert!(config.mouse.min_duration_ms <= config.mouse.max_duration_ms);
}
