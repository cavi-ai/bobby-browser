//! Golden lock: default Firefox companion profile matches create_session output.

use fingerprinting::{create_session, FingerprintConfig};
use serde_json::Value;
use std::path::PathBuf;

fn default_profile_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/firefox-companion/src/default-fingerprint-profile.json")
}

#[test]
fn default_profile_matches_rust_create_session() {
    let session = create_session(&FingerprintConfig::default().with_inject_chrome(false));
    let session_value = serde_json::to_value(&session).expect("session should serialize to JSON");

    let path = default_profile_path();
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing golden profile at {}: {e}\n\
             Regenerate: cargo test -p fingerprinting dump_default_fingerprint_profile -- --nocapture",
            path.display()
        )
    });
    let golden: Value = serde_json::from_str(&raw).unwrap_or_else(|e| {
        panic!(
            "invalid golden JSON at {}: {e}\n\
             Regenerate: cargo test -p fingerprinting dump_default_fingerprint_profile -- --nocapture",
            path.display()
        )
    });

    assert_eq!(
        session_value, golden,
        "default-fingerprint-profile.json drifted from FingerprintConfig::default().with_inject_chrome(false)\n\
         Regenerate: cargo test -p fingerprinting dump_default_fingerprint_profile -- --nocapture > {}",
        path.display()
    );

    assert_ne!(
        golden["canvasHash"].as_str().unwrap_or(""),
        "0".repeat(64),
        "canvasHash must not be placeholder zeros"
    );
    assert_ne!(
        golden["audioHash"].as_str().unwrap_or(""),
        "0".repeat(64),
        "audioHash must not be placeholder zeros"
    );
    assert_ne!(
        golden["webgl"]["hash"].as_str().unwrap_or(""),
        "0".repeat(64),
        "webgl.hash must not be placeholder zeros"
    );
    assert_eq!(golden["sessionSeed"], 0xb0b5f1d_u64);
}

#[test]
#[ignore = "helper: run with --nocapture to regenerate default-fingerprint-profile.json"]
fn dump_default_fingerprint_profile() {
    let session = create_session(&FingerprintConfig::default().with_inject_chrome(false));
    eprintln!(
        "{}",
        serde_json::to_string_pretty(&session).expect("serialize session")
    );
}
