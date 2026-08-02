//! Fingerprint toggle×navigate conformance for Firefox BiDi.
//!
//! The behavioral contract (preload add/remove on toggle, no double-add on
//! `open_page` when already synced) is covered by the always-on FakeBidi test
//! `fingerprint_toggle_adds_and_removes_preload_script` in `worker.rs`.
//!
//! A live Firefox probe mirroring `worker-pool/tests/fingerprint_conformance.rs`
//! would require a stable BiDi launch path in this crate; none exists yet.
