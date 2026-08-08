---
documentedVersion: 0.7.0
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

## Challenge levels

Level 1 is the default release gate above. It remains deterministic and does not load reCAPTCHA.

Level 2 is an opt-in training ground. Its seed fixes the obstacle layout while adding an accessible modal, a same-origin popup checkpoint, irregular field ordering, a delayed duplicate-looking email control, and Google reCAPTCHA v2. Start it with credentials registered for the hostname you will use:

```bash
export BOBBY_GAUNTLET_LEVEL=2
export BOBBY_GAUNTLET_RECAPTCHA_SITE_KEY="your-public-site-key"
export BOBBY_GAUNTLET_RECAPTCHA_SECRET="your-server-secret"
cargo test -p runtime-tests --test modern_gauntlet_e2e \
  level_two_recaptcha_training_ground -- --exact --nocapture
```

The command prints the isolated onboarding URL and keeps the scenario server running until Ctrl-C. The browser receives only the site key. The response token is verified through Google's `siteverify` endpoint before onboarding state changes; the secret and response token are excluded from run configuration, request logs, snapshots, and evidence.

The training ground intentionally includes no CAPTCHA solver or bypass. An automation agent must pause for legitimate completion of the challenge, then continue the workflow.

## Competitor gauntlet runner

`benchmarks/competitor-gauntlet` is a separate harness that drives the gauntlet
journeys with alternate tooling stacks.

Run it from the workspace tree:

```bash
cd benchmarks/competitor-gauntlet
cargo run -- --tool bobby
```

`--tool` is required.

- `--tool bobby` runs only the native bobby-browser runner.
- `--tool all` runs every configured runner, including the full bobby competitor
  gamut.
- Runner names are validated against `benchmarks/competitor-gauntlet/runners.json`.

When selecting bobby, the harness creates an isolated run workspace and writes
`bobby-gauntlet.toml` with:

- `upload_roots = ["./data/uploads"]`
- `downloads_dir = "./downloads"`
- `artifacts_dir = "./artifacts"`
- `[http] allow_loopback = true`
- `[mcp] startup_toolset = "full"`

The fixture is staged at `./data/uploads/approved-upload.txt` in that workspace
to satisfy upload policy, and `BOBBY_BROWSER_CONFIG` is pointed at that config.
`BOBBY_MCP_TOOLSET` is also set to `full` for that runner.

## Standalone scenario server

Out-of-process drivers (benchmarks, third-party tooling) can run the same
seeded scenario without the test harness:

```bash
cargo run -p gauntlet-server -- --seed demo
```

The binary prints the onboarding URL and serves until Ctrl-C. Drivers verify
outcomes over `GET /__gauntlet/snapshot` (run-scoped effect counts, onboarding
record, upload digests) and `GET /__gauntlet/request-log` — the same state the
in-process assertions use, so verification stays server-authoritative and
tool-neutral.

Each run uses isolated seeded server state. Passing requires the relevant UI state plus authoritative server counts, file digests, and runtime journal evidence; application-private scorecards are not accepted. Evidence bundles are written under `target/modern-gauntlet-artifacts/<journey>/<run-id>/` and CI uploads that directory when the gate fails.
