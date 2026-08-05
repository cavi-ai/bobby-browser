# Real Modern Gauntlet and Runtime E2E Design

**Date:** 2026-08-05

**Status:** Approved design; implementation not started

**Branch:** `test/real-modern-gauntlet-e2e`

## Purpose

Replace the artificial browser gauntlet with a deterministic but realistic web application and make five independent, mandatory, real-browser journeys the release proof for agent-facing runtime behavior.

The work is successful when regressions in navigation, semantic interaction, dynamic pages, uploads, frames, popups, recovery, downloads, or durable side effects fail CI through the same public runtime boundary used by agents.

## Evidence and Problem Statement

The current test architecture can report healthy behavior without proving the shipped experience:

- `packages/bobby-gauntlet/index.html` is a minimal application shell. The page is assembled from test-oriented controls and does not resemble a modern production site.
- Package tests execute the application primarily through JSDOM. They validate controller and DOM logic but do not prove browser/runtime integration.
- `crates/runtime-tests/tests/bobby_skills_gauntlet.rs` is approximately 1,900 lines and combines environment setup, browser orchestration, recovery, assertions, artifacts, and aggregate scoring.
- Its production championship test is ignored because it requires installed browser engines and a separately built gauntlet.
- The Chrome CI job runs selected ignored runtime tests but does not select `bobby_skills_gauntlet` and does not build the gauntlet bundle in that job.
- Application-generated scorecard scripts and test-specific selectors allow tests to observe implementation-only state unavailable on normal sites.
- A single championship aggregate makes failures harder to isolate and lets one synthetic course stand in for distinct user workflows.
- Browser prerequisites are communicated through environment variables and opt-in commands, so local, package, and CI test paths do not share one hermetic entry point.
- `docs/superpowers/PRD.md`, required by `AGENTS.md`, is absent even after running the repository seeding script. Implementation must remain within the paths named in this design unless the canonical PRD is restored and imposes narrower ownership.

## Scope

### In scope

- Replace the gauntlet presentation with a polished, responsive operations workspace.
- Preserve deterministic seeded scenarios behind normal application behavior.
- Create reusable real-browser E2E infrastructure.
- Add exactly five mandatory end-to-end release journeys, each independently runnable.
- Exercise the public runtime command path rather than directly calling gauntlet controllers.
- Assert visible state, durable server state, artifacts, and runtime evidence.
- Make CI build the production bundle and execute all five journeys in installed Chromium.
- Retain fast unit/component coverage for deterministic logic.
- Document local execution and failure artifacts.
- Remove the old championship release gate after temporary parity characterization.

### Out of scope

- General runtime refactoring unrelated to failures exposed by the five journeys.
- A general-purpose demo product, authentication service, or production backend.
- Pixel-perfect visual regression testing.
- Testing arbitrary third-party internet sites.
- Expanding the suite beyond the five required journeys in this change.
- Making Firefox a blocking gate in this first migration. Shared fixtures must remain engine-neutral so Firefox can adopt them later.

## Chosen Approach

Replace the gauntlet's presentation and E2E contract while keeping deterministic test data underneath it.

This is preferred over polishing the existing stations because isolated browser tricks still do not model real user work. It is preferred over adding a second application because two release gates would duplicate behavior and leave the weak aggregate authoritative.

The existing station/controller tests remain only where they provide useful characterization during migration. They are not the final release contract.

## Application Design

The replacement is a fictional operations workspace named **Northstar Ops**. It behaves like an ordinary modern SaaS application rather than a test harness.

### Information architecture

- Responsive application shell with sidebar navigation, top search, account menu, notification surface, and mobile navigation.
- Dashboard with summary cards, recent activity, actionable work queue, loading skeletons, and empty/error states.
- Customers list with search, filtering, sorting, pagination, and row navigation.
- Customer detail with overview, timeline, documents, and actions.
- Multi-step onboarding flow with semantic labels, dependent fields, validation, review, and confirmation.
- Documents area with upload progress, metadata, and an embedded preview.
- Integrations area with an external authorization popup flow.
- Reports area with background generation and file export.

### Realism without nondeterminism

Every run receives a seed. The seed selects stable record identifiers, names, ordering, delays, validation rules, and expected file contents. The seed is transported through ordinary session/API state, not exposed as a scorecard in the DOM.

The test server owns authoritative mutable state for the run. Browser-visible success alone is insufficient when the workflow claims persistence. State is isolated per run identifier so tests may execute independently or in parallel.

The UI uses production-style asynchronous boundaries:

- API-backed navigation and mutation;
- delayed search and loading states;
- DOM replacement after data refresh;
- optimistic update followed by server confirmation;
- client and server validation;
- modal and toast overlays;
- iframe document preview;
- separate-window authorization;
- file upload and download;
- recoverable transient failures.

The application must remain accessible: semantic headings and landmarks, associated labels, keyboard-operable controls, visible focus, useful names, live status announcements, and responsive layouts are product requirements, not test hooks.

## Architecture and Boundaries

### `packages/bobby-gauntlet`: application

Owns UI, routes, application state, and API client behavior. Components expose ordinary semantic HTML. Stable selectors are permitted only where a real application would reasonably expose identity; tests should prefer roles, labels, names, URLs, and visible state.

The current single-file rendering concentration in `src/app.ts` is split by responsibility:

- application shell and router;
- page modules;
- shared accessible components;
- API client and typed models;
- deterministic scenario configuration;
- visual styles and responsive tokens.

No module emits test scorecards or serializes controller internals into the page.

### Scenario server

A reusable Rust fixture serves the built application and scoped JSON/file endpoints. It owns seeded state, request logs, controlled fault injection, upload storage, authorization completion, report generation, and final-state queries.

Fault injection is declared at fixture creation. Tests do not mutate hidden state halfway through a workflow except through explicit fixture handles at a documented boundary, such as simulating a runtime process interruption.

### Runtime driver

A shared driver launches the production runtime and installed Chromium, creates a session/page, submits public runtime commands, and collects outcomes. Journey tests use task-level helpers only to reduce boilerplate; helpers may not bypass public commands or directly manipulate the page.

The driver owns:

- bounded startup and teardown;
- unique temporary state directories and ports;
- production bundle discovery/build contract;
- command deadlines;
- screenshots, DOM snapshots, console/network records, journals, and downloads;
- explicit cleanup on success and artifact preservation on failure.

### Evidence assertions

Assertions are divided into four independent classes:

1. Visible evidence: URL, accessible content, and rendered state.
2. Durable evidence: scenario-server state and absence of duplicate effects.
3. Artifact evidence: uploaded/downloaded bytes, media type, filename, and digest.
4. Runtime evidence: terminal command outcomes, journal history, recovery records, and bounded retries.

A journey passes only when all evidence classes relevant to it pass. Application-private scorecards are forbidden.

## Five Mandatory E2E Journeys

Each journey is one separately named test with its own seeded fixture, runtime session, timeout, and artifacts.

The required test names are:

- `customer_discovery_and_update_is_durable`;
- `validated_onboarding_preserves_accepted_values`;
- `document_upload_preview_and_confirmation_are_durable`;
- `popup_authorization_survives_obstruction`;
- `interrupted_report_recovers_once_and_downloads`.

### 1. Customer discovery and update

The agent opens the dashboard, navigates to Customers, searches by customer intent, waits through delayed results, opens the matching customer, edits its priority, and returns to the list.

The scenario replaces the result row after the delayed fetch so a cached DOM handle becomes stale. Success requires rediscovery of the stable target.

Proof:

- correct customer route reached;
- updated priority visible after returning to the list;
- server holds exactly one update with the expected value;
- journal shows successful navigation, inspection, and interaction outcomes.

### 2. Validated multi-step onboarding

The agent completes identity, organization, plan, and review steps using semantic meaning rather than field order. A dependent field appears after a plan selection. The server rejects one syntactically valid postal code while preserving accepted values; the agent corrects only the rejected value and submits.

Proof:

- validation feedback is surfaced and then cleared;
- accepted values survive correction;
- confirmation route and generated onboarding identifier are visible;
- one and only one onboarding record is persisted with all expected values.

### 3. Document upload and embedded preview

The agent opens a customer document tab, uploads an approved fixture, waits for processing, and opens its preview inside an iframe. It completes a confirmation action inside the embedded document.

Proof:

- upload progress reaches a terminal success state;
- stored bytes and SHA-256 match the fixture;
- metadata and customer association are correct;
- preview frame renders the expected document identity;
- embedded confirmation is persisted exactly once.

### 4. Popup authorization with obstruction handling

The agent starts an integration connection, completes authorization in a popup, returns to the opener, dismisses a notification/consent obstruction, and verifies the connected account.

Proof:

- popup navigation reaches the authorized callback;
- opener reflects the connected identity after asynchronous refresh;
- the obstruction is dismissed through a normal visible control;
- authorization state contains one grant and no leaked or duplicate popup session.

### 5. Interrupted recovery and report download

The agent configures a report and begins generation. The fixture/runtime is interrupted only after the durable execution boundary is proven. A replacement runtime resumes from the same journal, observes completion without replaying the mutation, downloads the report, and verifies it.

Proof:

- recovery starts from the durable checkpoint and reaches one terminal outcome;
- report generation has exactly one durable side effect;
- retry and recovery counts remain within declared bounds;
- downloaded filename, media type, bytes, and digest match server output;
- final UI shows the completed report.

## Error Handling and Diagnostics

- Every wait has a named condition and explicit deadline. Unbounded sleeps are forbidden.
- Startup failures distinguish missing bundle, missing browser, port binding, runtime launch, and readiness timeout.
- Command failures retain the structured runtime outcome instead of collapsing to a generic panic.
- Failed journeys write a run manifest containing seed, test name, runtime/browser versions, timestamps, and artifact paths.
- Failure artifacts include the last screenshot, accessible/DOM snapshot, runtime journal, browser console, relevant network log, scenario request log, and server-state snapshot.
- Secrets or host-specific absolute paths must not appear in committed fixtures or snapshots.
- Teardown errors are reported without hiding the primary failure.

## Test Strategy

### Test layers

- Unit tests cover scenario generation, validation, state transitions, artifact hashing, and API contracts.
- Browser component tests cover accessibility and UI state transitions but do not count as runtime E2E proof.
- The five installed-Chromium journeys are the mandatory system tests.
- Temporary characterization coverage for the old stations may remain during migration, clearly labeled non-gating and deleted once the five journeys pass consistently.

### Anti-cheating constraints

Journey code must not:

- call `GauntletController` or station verification functions;
- execute page JavaScript to set application state or trigger actions;
- read hidden scorecard scripts or test-only global variables;
- query the scenario server for target discovery before the runtime finds it;
- use arbitrary sleeps as synchronization;
- accept a screenshot alone as proof of persistence;
- share mutable state between journeys.

### CI contract

The Chrome job performs these steps in order:

1. install locked Rust and Node dependencies;
2. build the production gauntlet bundle;
3. build the runtime test binary;
4. locate and smoke-test Chromium;
5. run all five named E2E tests with one browser test thread unless isolation is proven safe;
6. upload diagnostic artifacts on failure.

The job fails when a named test is ignored, filtered out, discovers zero tests, cannot find the production bundle, or lacks Chromium. There is no success path that silently skips the suite.

A manifest/list test asserts that the release suite contains exactly the five required journey identifiers. This prevents accidental deletion or filtering.

## Migration Sequence

1. Extract reusable fixture, runtime driver, and evidence helpers from the monolithic gauntlet test without changing runtime behavior.
2. Implement the scenario server and typed application API.
3. Build the Northstar Ops shell, pages, responsive styling, and deterministic states.
4. Implement the five journey tests one at a time, using failing tests to drive missing product/runtime behavior.
5. Add the explicit release-suite manifest and CI invocation.
6. Run the old championship as temporary characterization and compare capability coverage.
7. Delete the old championship release test and application scorecard plumbing after all five replacements pass.
8. Update the gauntlet guide with the new local and CI commands and diagnostic locations.

## Expected Write Scope

Implementation is expected to touch only:

- `packages/bobby-gauntlet/**`;
- `crates/runtime-tests/**`;
- a narrowly scoped reusable fixture under `crates/test-site/**` if the existing crate is the correct home;
- `.github/workflows/ci.yml`;
- `docs/bobby-browser/source/pages/guides/gauntlet.md` and generated/versioned copies required by the repository's documentation workflow;
- workspace manifests or lockfiles only when mechanically required by those changes.

Runtime production crates may be changed only when a failing mandatory journey demonstrates a concrete defect. Such changes require a focused regression test at the owning crate boundary in addition to the E2E proof.

## Acceptance Criteria

- The application looks and behaves like a responsive modern operations product at desktop and mobile widths.
- No application scorecard, controller receipt, or test-only state is required for E2E assertions.
- Five independently runnable installed-Chromium journeys pass through the public runtime boundary.
- Each journey verifies relevant visible, durable, artifact, and runtime evidence.
- Each journey has bounded synchronization and produces actionable failure artifacts.
- CI explicitly builds the production site and runs all five tests without ignored-test semantics.
- CI detects missing, skipped, or zero-test release suites.
- Unit/component tests and Rust tests covering new infrastructure pass.
- The legacy championship gate and obsolete scorecard plumbing are removed after parity is established.
- Documentation describes one reproducible local command and the CI contract.

## Verification Commands

Verification will include:

```bash
pnpm --filter @cavi-ai/bobby-gauntlet test
pnpm --filter @cavi-ai/bobby-gauntlet build
cargo test -p runtime-tests --test modern_gauntlet_e2e --locked -- --list
cargo test -p runtime-tests --test modern_gauntlet_e2e --locked -- --test-threads=1
cargo test -p runtime-tests --locked
```

The live E2E command must run all five tests under the same browser environment used by CI. Any environment-specific wrapper belongs in the implementation plan and repository scripts, not in undocumented operator knowledge.

## Risks and Controls

- **Flakiness from asynchronous UI:** synchronize on observable application/runtime conditions and bounded deadlines; never timing guesses.
- **Tests coupled to styling:** prefer accessible semantics and durable state over CSS structure.
- **Fixture becoming another fake controller:** expose ordinary HTTP behavior and server state; do not publish expected answers to the browser or runtime.
- **One oversized replacement test:** enforce one file/module per journey plus focused shared helpers.
- **Runtime defects expanding scope:** require the failing journey and a crate-level regression before production changes.
- **Slow CI:** build once, isolate state per test, and reuse immutable artifacts without sharing browser/session state.
- **Cross-engine lock-in:** keep fixture and assertions engine-neutral; Chromium is the initial blocking implementation.

## Decisions

- The modern application replaces, rather than supplements, the old gauntlet release contract.
- Deterministic seeded behavior remains, but hidden scorecards and internal-controller assertions do not.
- Chromium is the required first blocking engine; Firefox adoption is follow-up work.
- The suite contains five independently diagnosable journeys, not one aggregate championship.
- Public runtime commands and durable effects are the authoritative proof of behavior.
