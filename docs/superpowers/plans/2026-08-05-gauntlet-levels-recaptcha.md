# Bobby Gauntlet Levels and reCAPTCHA Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Preserve the current Northstar gauntlet as Level 1 and add a seeded Level 2 with irregular onboarding, additional interruptions, and a real reCAPTCHA v2 gate.

**Architecture:** `ScenarioConfig` is the authoritative level/trap configuration. A small public run-config endpoint exposes only browser-safe settings; the TypeScript application consumes that immutable configuration and posts the reCAPTCHA response with onboarding. An injected Rust verifier isolates Google `siteverify` I/O from deterministic tests.

**Tech Stack:** Rust, Axum, Tokio, Reqwest, TypeScript, DOM APIs, Google reCAPTCHA v2.

## Global Constraints

- Missing level means Level 1 and preserves existing journeys.
- Level 2 trap selection is deterministic for `(level, seed, journey)`.
- Level 2 live execution requires `BOBBY_GAUNTLET_RECAPTCHA_SITE_KEY` and `BOBBY_GAUNTLET_RECAPTCHA_SECRET`.
- The site key may reach the browser; the secret and response tokens must never enter logs, snapshots, or evidence.
- No CAPTCHA-solving or bypass logic is added.
- Rejected, missing, expired, duplicate, or unverifiable CAPTCHA responses create no customer.

---

### Task 1: Level and seeded trap contract

**Files:**
- Modify: `crates/runtime-tests/tests/modern_gauntlet/scenario.rs`
- Test: `crates/runtime-tests/tests/modern_gauntlet/scenario.rs`

**Interfaces:**
- Produces: `GauntletLevel::{One, Two}`, `LevelTwoTrapPlan::seeded(&str)`, `ScenarioConfig::level_two(...)`.
- Produces: `GET /api/run-config` returning `{level, seed, traps, recaptchaSiteKey}` without secrets.

- [ ] **Step 1: Write failing unit tests**

Add tests proving default Level 1, deterministic Level 2 trap selection, unknown-level rejection, missing-key failure, and public configuration redaction:

```rust
assert_eq!(ScenarioConfig::seeded("atlas").level, GauntletLevel::One);
assert_eq!(LevelTwoTrapPlan::seeded("atlas"), LevelTwoTrapPlan::seeded("atlas"));
assert!(!serde_json::to_string(&public_config).unwrap().contains("secret-canary"));
```

- [ ] **Step 2: Run the focused tests and witness RED**

Run: `CARGO_BUILD_RUSTC_WRAPPER= cargo test -p runtime-tests --test modern_gauntlet_e2e modern_gauntlet::scenario::tests --offline`

Expected: compile failure because the level types do not exist.

- [ ] **Step 3: Implement the level contract and endpoint**

Add bounded enums/records and store browser-safe configuration in `SharedState`. Build `application_url` with `run` and `level`. Register `/api/run-config` before the static fallback.

- [ ] **Step 4: Run focused tests and verify GREEN**

Run the command from Step 2. Expected: all scenario unit tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/runtime-tests/tests/modern_gauntlet/scenario.rs
git commit -m "feat(gauntlet): add deterministic challenge levels"
```

### Task 2: Browser run configuration and irregular Level 2 onboarding

**Files:**
- Create: `packages/bobby-gauntlet/src/run-config.ts`
- Create: `packages/bobby-gauntlet/src/recaptcha.ts`
- Modify: `packages/bobby-gauntlet/src/app.ts`
- Modify: `packages/bobby-gauntlet/src/api.ts`
- Modify: `packages/bobby-gauntlet/src/models.ts`
- Modify: `packages/bobby-gauntlet/src/pages/onboarding.ts`
- Test: `packages/bobby-gauntlet/test/api.test.ts`
- Test: `packages/bobby-gauntlet/test/northstar-browser.test.ts`

**Interfaces:**
- Consumes: `GET /api/run-config` from Task 1.
- Produces: `RunConfig`, `RecaptchaController`, and `NorthstarApi.onboard(input, recaptchaResponse?)`.

- [ ] **Step 1: Write failing TypeScript tests**

Prove Level 1 never loads reCAPTCHA, Level 2 renders the configured site key, field order/traps are stable for a seed, delayed controls appear, and onboarding JSON contains `recaptchaResponse` only for Level 2.

```ts
assert.equal(document.querySelector("[data-recaptcha-site-key='site-test']") !== null, true);
assert.deepEqual(JSON.parse(await request.text()), { ...onboarding, recaptchaResponse: "token-test" });
```

- [ ] **Step 2: Run package tests and witness RED**

Run: `pnpm --filter @cavi-ai/bobby-gauntlet test`

Expected: failures because run configuration and reCAPTCHA rendering do not exist.

- [ ] **Step 3: Implement browser configuration and form variance**

Fetch run configuration once at mount. Pass it to page factories. Implement a small reCAPTCHA adapter that loads `https://www.google.com/recaptcha/api.js?render=explicit`, calls `grecaptcha.render`, and reads the response through the widget callback. Level 2 varies order, adds a conditional similar-label control, and inserts one control after a bounded seeded delay.

- [ ] **Step 4: Run package tests and verify GREEN**

Run: `pnpm --filter @cavi-ai/bobby-gauntlet test`

- [ ] **Step 5: Commit**

```bash
git add packages/bobby-gauntlet
git commit -m "feat(gauntlet): add irregular level two onboarding"
```

### Task 3: Server-side reCAPTCHA verification

**Files:**
- Modify: `crates/runtime-tests/Cargo.toml`
- Modify: `crates/runtime-tests/tests/modern_gauntlet/scenario.rs`
- Test: `crates/runtime-tests/tests/modern_gauntlet/scenario.rs`

**Interfaces:**
- Consumes: `recaptchaResponse` emitted by Task 2.
- Produces: async `RecaptchaVerifier::verify(&str) -> Result<RecaptchaDecision, RecaptchaVerifyError>` and a Google-backed implementation with a bounded timeout.

- [ ] **Step 1: Write failing verifier and mutation tests**

Use a deterministic fake verifier to prove missing/invalid/upstream-error responses create zero records and an accepted response creates exactly one.

```rust
assert_eq!(server.snapshot().await.onboarding_records, 0);
assert_eq!(server.snapshot().await.recaptcha_verified, Some(false));
```

- [ ] **Step 2: Run focused tests and witness RED**

Run: `CARGO_BUILD_RUSTC_WRAPPER= cargo test -p runtime-tests --test modern_gauntlet_e2e modern_gauntlet::scenario::tests --offline`

- [ ] **Step 3: Implement verification before mutation**

Deserialize the CAPTCHA field separately from `OnboardingRecord`. Verify before acquiring the mutation lock. Map invalid responses to stable `invalid_challenge` and transport/timeouts to `challenge_unavailable`. Retain only a boolean/category counter and never the submitted token.

- [ ] **Step 4: Run Rust and package regression suites**

Run:

```bash
CARGO_BUILD_RUSTC_WRAPPER= cargo test -p runtime-tests --test modern_gauntlet_e2e modern_gauntlet::scenario::tests --offline
pnpm --filter @cavi-ai/bobby-gauntlet test
```

- [ ] **Step 5: Commit**

```bash
git add crates/runtime-tests/Cargo.toml crates/runtime-tests/tests/modern_gauntlet/scenario.rs packages/bobby-gauntlet
git commit -m "feat(gauntlet): verify level two reCAPTCHA"
```

### Task 4: Level 2 interruptions, live journey, and documentation

**Files:**
- Create: `packages/bobby-gauntlet/src/traps.ts`
- Modify: `packages/bobby-gauntlet/src/app.ts`
- Modify: `packages/bobby-gauntlet/src/styles.css`
- Modify: `packages/bobby-gauntlet/test/northstar-browser.test.ts`
- Modify: `crates/runtime-tests/tests/modern_gauntlet_e2e.rs`
- Modify: `docs/bobby-browser/source/pages/guides/gauntlet.md`

**Interfaces:**
- Consumes: `RunConfig.traps` and reCAPTCHA-gated onboarding.
- Produces: deterministic modal/popup trap rendering and a separately selectable live Level 2 journey.

- [ ] **Step 1: Write failing interruption and live-gate contract tests**

Assert Level 1 has no extra interruption; Level 2 renders the selected modal and popup at stable boundaries; the live journey refuses to start without both keys.

- [ ] **Step 2: Run focused tests and witness RED**

Run:

```bash
pnpm --filter @cavi-ai/bobby-gauntlet test
CARGO_BUILD_RUSTC_WRAPPER= cargo test -p runtime-tests --test modern_gauntlet_e2e release_suite_names_are_stable --offline
```

- [ ] **Step 3: Implement interruptions and live Level 2 selection**

Render accessible, dismissible modal traps and same-origin popup traps from the deterministic plan. Add a live Level 2 onboarding test that is opt-in through its explicit test name and environment requirements; do not change the five Level 1 names.

- [ ] **Step 4: Document commands and run final gates**

Document Level 1 default behavior, Level 2 environment variables, and exact commands. Run:

```bash
pnpm --filter @cavi-ai/bobby-gauntlet test
pnpm --filter @cavi-ai/bobby-gauntlet build
CARGO_BUILD_RUSTC_WRAPPER= cargo test -p runtime-tests --test modern_gauntlet_e2e --offline
CARGO_BUILD_RUSTC_WRAPPER= cargo clippy --workspace --all-targets --offline -- -D warnings
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 5: Commit**

```bash
git add packages/bobby-gauntlet crates/runtime-tests docs/bobby-browser/source/pages/guides/gauntlet.md
git commit -m "test(gauntlet): add level two obstacle course"
```
