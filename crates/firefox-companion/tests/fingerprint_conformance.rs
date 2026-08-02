//! Fingerprint toggle×navigate conformance for Firefox BiDi.
//!
//! The behavioral contract (preload add/remove on toggle, no double-add on
//! `open_page` when already synced) is covered by the always-on FakeBidi test
//! `fingerprint_toggle_adds_and_removes_preload_script` in `worker.rs`.
//!
//! Live Firefox proof for companion enrollment + native input lives in
//! `runtime-tests` (`tests/firefox_companion.rs`, `tests/behavioral_firefox.rs`)
//! using `InstalledFirefoxConfig` / `BOBBY_FIREFOX_*` — same path as gauntlet.
