//! Live Firefox behavioral dogfood (ignored without installed Firefox).
//!
//! Same env contract as `firefox_companion` / gauntlet:
//! `BOBBY_FIREFOX_BIN`, `BOBBY_FIREFOX_PROFILE`, `BOBBY_COMPANION_EXTENSION`.
//!
//! Run:
//! ```text
//! scripts/dev/behavioral-firefox.sh
//! # or
//! make behavioral-dogfood
//! ```

use companion_protocol::InteractionPath;
use runtime_tests::{run_installed_firefox_behavioral_dogfood, InstalledFirefoxConfig};

#[tokio::test]
#[ignore = "requires installed headed Firefox + companion profile (BOBBY_FIREFOX_*)"]
async fn installed_firefox_behavioral_dogfood_passes() {
    let config = InstalledFirefoxConfig::from_env().expect("installed Firefox test configuration");
    let report = run_installed_firefox_behavioral_dogfood(config)
        .await
        .expect("Firefox behavioral dogfood");

    eprintln!(
        "behavioral dogfood: type_ms={} click_ms={} probe={}",
        report.type_duration_ms, report.click_duration_ms, report.probe
    );

    assert_eq!(report.confirmation_text, "Submitted");
    assert_eq!(report.type_interaction_path, InteractionPath::EngineNative);
    assert_eq!(report.click_interaction_path, InteractionPath::EngineNative);

    // Behavioral typing (clear + key stream) must not complete as an instant CDP dump.
    assert!(
        report.type_duration_ms >= 400,
        "type_text finished too quickly ({}ms); expected human-paced BiDi keys",
        report.type_duration_ms
    );
    // Approach path + hover dwell + click should take measurable wall time.
    assert!(
        report.click_duration_ms >= 100,
        "click finished too quickly ({}ms); expected curved pointer path",
        report.click_duration_ms
    );

    assert_eq!(report.probe["passed"], true);
    let keydowns = report.probe["keydowns"].as_u64().unwrap_or(0);
    assert!(
        keydowns >= 5,
        "probe saw too few keydowns ({keydowns}); BiDi typing may not be reaching the page"
    );
    let key_max = report.probe["keyIntervalMaxMs"].as_u64().unwrap_or(0);
    assert!(
        key_max >= 20,
        "key interval max {key_max}ms looks like an instantaneous dump"
    );
    let pointer_moves = report.probe["pointerMoves"].as_u64().unwrap_or(0);
    assert!(
        pointer_moves >= 3,
        "probe saw too few pointerMoves ({pointer_moves}); approach path may not be firing DOM events"
    );
}
