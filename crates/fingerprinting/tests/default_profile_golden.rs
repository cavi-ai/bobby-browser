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

/// Review-pinned identity of the default profile: the golden must match both
/// `create_session` AND these literals, so regenerating the golden from
/// drifted code cannot bless a contradiction. Changing a literal here is a
/// deliberate, reviewed identity change -- say why in the commit.
#[test]
fn default_profile_identity_is_the_reviewed_windows_chrome() {
    let path = default_profile_path();
    let golden: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("golden profile reads"))
            .expect("golden profile parses");

    assert_eq!(
        golden["userAgent"],
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"
    );
    assert_eq!(golden["platform"], "Win32");
    assert_eq!(golden["locale"], "en-US");
    assert_eq!(golden["timezoneId"], "America/New_York");
    assert_eq!(golden["hardwareConcurrency"], 8);
    assert_eq!(golden["deviceMemory"], 8);
    assert_eq!(golden["maxTouchPoints"], 0);
    assert_eq!(golden["webgl"]["vendor"], "Google Inc. (NVIDIA)");
    assert_eq!(
        golden["webgl"]["renderer"],
        "ANGLE (NVIDIA, NVIDIA GeForce GTX 1660 Super Direct3D11 vs_5_0 ps_5_0, D3D11)"
    );
    assert_eq!(golden["webgl"]["maxTextureSize"], 16384);
    assert_eq!(golden["screenResolution"]["width"], 1920);
    assert_eq!(golden["screenResolution"]["height"], 1080);
    assert_eq!(golden["screenResolution"]["colorDepth"], 24);
    assert_eq!(golden["clientHints"]["platform"], "Windows");
    assert_eq!(golden["clientHints"]["fullVersion"], "131.0.0.0");
    assert_eq!(golden["clientHints"]["mobile"], false);
    // No automation tells anywhere in the identity.
    let serialized = golden.to_string();
    for tell in ["Headless", "webdriver", "Automation", "HeadlessChrome"] {
        assert!(
            !serialized.contains(tell),
            "default profile leaks automation tell {tell:?}"
        );
    }
}
