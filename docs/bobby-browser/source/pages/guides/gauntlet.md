---
documentedVersion: {{PRODUCT_VERSION}}
---

# Northstar browser release gate

`@cavi-ai/bobby-gauntlet` is the Northstar Ops application: a responsive, API-backed customer operations workspace. It replaces the synthetic station/championship course. Package tests cover application contracts; five non-ignored installed-Chromium journeys prove the public runtime boundary, visible state, durable effects, recovery, uploads, frames, popups, and downloads.

Build and test the production application:

```bash
pnpm --filter @cavi-ai/bobby-gauntlet test
pnpm --filter @cavi-ai/bobby-gauntlet build
```

Run the complete release gate with an installed Chrome or Chromium:

```bash
export BOBBY_CHROME_EXECUTABLE="/path/to/chrome"
cargo test -p runtime-tests --locked --test modern_gauntlet_e2e -- --test-threads=1
```

The five mandatory journeys are customer discovery and durable priority update, validated onboarding with preserved values, document upload with iframe confirmation, popup authorization with obstruction handling, and interrupted report recovery with a verified download. None is ignored, and a manifest test protects their stable names.

Each run uses isolated seeded server state. Passing requires the relevant UI state plus authoritative server counts, file digests, and runtime journal evidence; application-private scorecards are not accepted. Evidence bundles are written under `target/modern-gauntlet-artifacts/<journey>/<run-id>/` and CI uploads that directory when the gate fails.
