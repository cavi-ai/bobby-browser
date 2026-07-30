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

The production-runtime championship is intentionally ignored by the ordinary test suite because it needs an installed browser. Firefox is the default and does not fall back to Chromium. Build and install the companion native host once:

```bash
export BOBBY_FIREFOX_BIN="/path/to/firefox"
export BOBBY_FIREFOX_PROFILE="/path/to/dedicated-profile"
export BOBBY_COMPANION_EXTENSION="$PWD/packages/firefox-companion/dist"
scripts/dev/firefox-companion.sh
```

Then run the championship with the same three variables:

```bash
cargo test -p runtime-tests --test bobby_skills_gauntlet \
  production_bobby_passes_seeded_championship_with_replayable_evidence \
  -- --ignored --exact --nocapture
```

The gate launches Firefox, opens its WebDriver BiDi endpoint, installs the companion into the dedicated profile, enrolls the profile, and tears the browser down after the run. Set `BOBBY_CHAMPIONSHIP_HEADED=1` to watch the run and `BOBBY_CHAMPIONSHIP_SEED` to select a fixed release sample.

Chromium remains available only through explicit opt-in: set `BOBBY_CHAMPIONSHIP_ENGINE=chromium` and, when needed, `BOBBY_CHROMIUM_EXECUTABLE`.

Successful runs retain a redacted replayable scorecard and ten screenshot artifacts below `target/bobby-championship/<engine>/<seed>/`. The gate fails closed on pairing, capability, station, screenshot-integrity, manifest-replay, or redaction failures. Package tests establish deterministic behavior but do not replace live-engine release proof.
