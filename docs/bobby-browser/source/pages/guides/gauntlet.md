---
documentedVersion: 0.2.0
---

# Bobby browser gauntlet

`@bobby-browser/gauntlet` is a deterministic static application with ten isolated browser stations and a championship route. A version, seed, and difficulty produce an immutable manifest; every result is controller-verified and bound to that manifest. The course covers redirects, DOM replacement, semantic forms, validation, iframes, shadow roots, popups, file attachment, downloads, and a combined championship submission.

Run the deterministic contract suite and build the static course:

```bash
pnpm --filter @bobby-browser/gauntlet test
pnpm --filter @bobby-browser/gauntlet build
```

The production-runtime championship is intentionally ignored by the ordinary test suite because it needs an installed browser. Run Chromium with:

```bash
cargo test -p runtime-tests --test bobby_skills_gauntlet -- --ignored --nocapture
```

Set `BOBBY_CHROMIUM_EXECUTABLE` if Chromium is not in a standard macOS location, `BOBBY_CHAMPIONSHIP_HEADED=1` to watch the run, and `BOBBY_CHAMPIONSHIP_SEED` to select a fixed release sample.

For Firefox, start a dedicated test profile with the companion and WebDriver BiDi endpoint, then set `BOBBY_CHAMPIONSHIP_ENGINE=firefox`, `BOBBY_FIREFOX_PROFILE_ID`, `BOBBY_FIREFOX_BIDI_URL`, `BOBBY_FIREFOX_PROFILE_DIR`, and `BOBBY_FIREFOX_DESCRIPTOR_PATH`.

Successful runs retain a redacted replayable scorecard and ten screenshot artifacts below `target/bobby-championship/<engine>/<seed>/`. The gate fails closed on pairing, capability, station, screenshot-integrity, manifest-replay, or redaction failures. Package tests establish deterministic behavior but do not replace live-engine release proof.
