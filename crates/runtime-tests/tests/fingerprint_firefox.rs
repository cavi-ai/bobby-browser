//! Live Firefox fingerprint collector dogfood (ignored without installed Firefox).
//!
//! Same env contract as `firefox_companion` / behavioral dogfood:
//! `BOBBY_FIREFOX_BIN`, `BOBBY_FIREFOX_PROFILE`, `BOBBY_COMPANION_EXTENSION`.
//!
//! Run:
//! ```text
//! scripts/dev/fingerprint-firefox.sh
//! # or
//! make fingerprint-collectors-firefox
//! ```

use runtime_tests::{run_installed_firefox_fingerprint_dogfood, InstalledFirefoxConfig};

#[tokio::test]
#[ignore = "requires installed headed Firefox + companion profile (BOBBY_FIREFOX_*)"]
async fn installed_firefox_fingerprint_collector_dogfood_passes() {
    let config = InstalledFirefoxConfig::from_env().expect("installed Firefox test configuration");
    let report = run_installed_firefox_fingerprint_dogfood(config)
        .await
        .expect("Firefox fingerprint collector dogfood");

    let creepjs = report
        .reports
        .iter()
        .find(|r| r["site"] == "creepjs")
        .expect("creepjs report");
    eprintln!(
        "Firefox CreepJS scores: like={:?} headless={:?} stealth={:?} webDriverIsOn={:?} prefersLight={:?}",
        creepjs["headlessScores"]["like"],
        creepjs["headlessScores"]["headless"],
        creepjs["headlessScores"]["stealth"],
        creepjs["headlessFlags"]["webDriverIsOn"],
        creepjs["headlessFlags"]["prefersLightColor"],
    );
    assert_eq!(
        creepjs["headlessFlags"]["webDriverIsOn"], false,
        "webDriverIsOn must be false"
    );
    assert_eq!(
        creepjs["headlessFlags"]["hasHeadlessUA"], false,
        "hasHeadlessUA must be false"
    );
    assert_eq!(
        creepjs["stealthFlags"]["hasToStringProxy"], false,
        "hasToStringProxy must be false"
    );
    // navigator.webdriver value must be cleared even if CreepJS scores the redefine.
    assert!(
        creepjs["webdriver"].is_null() || creepjs["webdriver"] == false,
        "navigator.webdriver value must be false"
    );
}
