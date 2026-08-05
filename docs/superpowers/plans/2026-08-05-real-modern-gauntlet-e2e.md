# Real Modern Gauntlet and Runtime E2E Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the synthetic gauntlet with a responsive operations application and five mandatory installed-Chromium journeys that prove public runtime behavior, durable effects, artifacts, and recovery.

**Architecture:** `packages/bobby-gauntlet` becomes the Northstar Ops browser application, backed by a run-scoped deterministic HTTP scenario server in `runtime-tests`. A reusable runtime driver executes public commands against installed Chromium and records evidence. Five independent journey tests form the explicit CI release suite; unit/browser tests retain fast coverage below it.

**Tech Stack:** TypeScript 7, browser DOM APIs, esbuild, Node test runner with JSDOM, Rust 2021, Tokio, Axum, existing Bobby runtime crates, installed Chromium, GitHub Actions.

## Global Constraints

- The release suite contains exactly five independently runnable installed-Chromium journeys.
- Tests use public runtime commands; they do not call gauntlet controllers, execute JavaScript to mutate application state, or read hidden scorecards.
- Every wait has a named observable condition and explicit deadline; arbitrary sleeps are forbidden.
- Each run has isolated seeded mutable state and verifies all relevant visible, durable, artifact, and runtime evidence.
- Chromium is the required blocking engine; shared scenario fixtures remain engine-neutral.
- No new runtime production dependency is added unless a failing journey proves it necessary.
- Production runtime changes require both the failing E2E and a focused owning-crate regression test.
- Implementation writes remain inside the paths approved by the design spec.
- Failed journeys preserve a run manifest, screenshot, DOM/accessible snapshot, journal, console/network data when available, request log, and server-state snapshot.
- The production gauntlet bundle must be built before the E2E binary runs; missing browser, bundle, or tests is a hard failure.

---

## File Map

### Browser application

- `packages/bobby-gauntlet/src/models.ts`: API and domain types shared by pages.
- `packages/bobby-gauntlet/src/api.ts`: typed HTTP client and structured errors.
- `packages/bobby-gauntlet/src/router.ts`: history routing and route matching.
- `packages/bobby-gauntlet/src/components.ts`: accessible shared UI primitives.
- `packages/bobby-gauntlet/src/pages/dashboard.ts`: dashboard page.
- `packages/bobby-gauntlet/src/pages/customers.ts`: customer list/detail and priority mutation.
- `packages/bobby-gauntlet/src/pages/onboarding.ts`: multi-step validated onboarding.
- `packages/bobby-gauntlet/src/pages/documents.ts`: upload and iframe preview.
- `packages/bobby-gauntlet/src/pages/integrations.ts`: popup authorization and obstruction.
- `packages/bobby-gauntlet/src/pages/reports.ts`: report generation and download.
- `packages/bobby-gauntlet/src/app.ts`: application composition only.
- `packages/bobby-gauntlet/src/styles.css`: responsive visual system.
- `packages/bobby-gauntlet/index.html`: production shell and metadata.
- `packages/bobby-gauntlet/test/app-browser.test.ts`: application/browser behavior tests.
- `packages/bobby-gauntlet/test/api.test.ts`: API error and request tests.
- `packages/bobby-gauntlet/package.json`: build CSS and application bundle.

### Runtime E2E

- `crates/runtime-tests/tests/modern_gauntlet_e2e.rs`: five named release tests only.
- `crates/runtime-tests/tests/modern_gauntlet/scenario.rs`: seeded HTTP server and durable state.
- `crates/runtime-tests/tests/modern_gauntlet/driver.rs`: runtime/Chromium lifecycle and public-command helpers.
- `crates/runtime-tests/tests/modern_gauntlet/evidence.rs`: artifact capture and evidence assertions.
- `crates/runtime-tests/tests/modern_gauntlet/mod.rs`: shared exports and required-suite manifest.
- `crates/runtime-tests/tests/fixtures/approved-upload.txt`: deterministic upload fixture.
- `crates/runtime-tests/Cargo.toml`: only dependencies required by the shared fixture.

### Release wiring and docs

- `.github/workflows/ci.yml`: build the site and run the five-test suite without ignored semantics.
- `docs/bobby-browser/source/pages/guides/gauntlet.md`: local command, suite contract, diagnostics.
- `docs/bobby-browser/v0.6.0/guides/gauntlet.md`: generated/versioned documentation mirror if required by the docs build.
- `crates/runtime-tests/tests/bobby_skills_gauntlet.rs`: delete after replacement parity passes.
- `packages/bobby-gauntlet/src/controller.ts`, `manifest.ts`, `scorecard.ts`, `station.ts`, and `stations/**`: delete after package tests move to application behavior.

---

### Task 1: Typed application API and deterministic contracts

**Files:**
- Create: `packages/bobby-gauntlet/src/models.ts`
- Create: `packages/bobby-gauntlet/src/api.ts`
- Create: `packages/bobby-gauntlet/test/api.test.ts`
- Modify: `packages/bobby-gauntlet/src/index.ts`

**Interfaces:**
- Produces: `NorthstarApi` with `dashboard()`, `customers(query)`, `customer(id)`, `updatePriority(id, priority)`, `onboard(input)`, `uploadDocument(customerId, file)`, `integrationState()`, `completeAuthorization(code)`, `createReport(input)`, and `report(id)`.
- Produces: `ApiError { status: number; code: string; message: string; fields: Record<string,string> }`.
- Consumes: ordinary same-origin JSON and multipart endpoints implemented by Task 4.

- [ ] **Step 1: Write failing API tests**

```ts
test("request sends run identity and decodes structured field errors", async () => {
  const calls: Request[] = [];
  const api = new NorthstarApi("run-17", async (input, init) => {
    calls.push(new Request(input, init));
    return Response.json({ code: "postal_rejected", message: "Use 10001", fields: { postalCode: "Use 10001" } }, { status: 422 });
  });
  await assert.rejects(api.onboard(validOnboarding()), (error: ApiError) => {
    assert.equal(error.status, 422);
    assert.deepEqual(error.fields, { postalCode: "Use 10001" });
    return true;
  });
  assert.equal(calls[0].headers.get("x-northstar-run"), "run-17");
});
```

- [ ] **Step 2: Run the focused test and confirm the missing-module failure**

Run: `pnpm --filter @cavi-ai/bobby-gauntlet test -- api.test.ts`

Expected: FAIL because `NorthstarApi` and the typed models do not exist.

- [ ] **Step 3: Implement domain types and the HTTP client**

```ts
export class ApiError extends Error {
  constructor(readonly status: number, readonly code: string, message: string, readonly fields: Record<string, string> = {}) {
    super(message);
  }
}

export class NorthstarApi {
  constructor(readonly runId: string, private readonly fetcher: typeof fetch = fetch) {}
  async onboard(input: OnboardingInput): Promise<OnboardingReceipt> {
    return this.request("/api/onboarding", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(input) });
  }
  private async request<T>(path: string, init: RequestInit = {}): Promise<T> {
    const headers = new Headers(init.headers);
    headers.set("x-northstar-run", this.runId);
    const response = await this.fetcher(path, { ...init, headers });
    const body = await response.json();
    if (!response.ok) throw new ApiError(response.status, body.code, body.message, body.fields);
    return body as T;
  }
}
```

- [ ] **Step 4: Run package typecheck and API tests**

Run: `pnpm --filter @cavi-ai/bobby-gauntlet typecheck && pnpm --filter @cavi-ai/bobby-gauntlet test -- api.test.ts`

Expected: PASS.

- [ ] **Step 5: Commit the API contract**

```bash
git add packages/bobby-gauntlet/src/models.ts packages/bobby-gauntlet/src/api.ts packages/bobby-gauntlet/src/index.ts packages/bobby-gauntlet/test/api.test.ts
git commit -m "feat(gauntlet): add Northstar application API"
```

### Task 2: Responsive application shell and customer journey

**Files:**
- Create: `packages/bobby-gauntlet/src/router.ts`
- Create: `packages/bobby-gauntlet/src/components.ts`
- Create: `packages/bobby-gauntlet/src/pages/dashboard.ts`
- Create: `packages/bobby-gauntlet/src/pages/customers.ts`
- Create: `packages/bobby-gauntlet/src/styles.css`
- Create: `packages/bobby-gauntlet/test/app-browser.test.ts`
- Modify: `packages/bobby-gauntlet/src/app.ts`
- Modify: `packages/bobby-gauntlet/index.html`
- Modify: `packages/bobby-gauntlet/package.json`

**Interfaces:**
- Consumes: `NorthstarApi` and models from Task 1.
- Produces: `mountNorthstar(root: HTMLElement, api: NorthstarApi): NorthstarApp` and `NorthstarApp.navigate(path: string): Promise<void>`.
- Produces: semantic dashboard, customer list, customer detail, and priority mutation routes.

- [ ] **Step 1: Write failing shell and customer browser tests**

```ts
test("customer search replaces loading content and persists priority", async () => {
  const { document, api } = northstarDom();
  const app = mountNorthstar(document.querySelector("#app")!, api);
  await app.navigate("/customers");
  document.querySelector<HTMLInputElement>("[aria-label='Search customers']")!.value = "Atlas";
  document.querySelector<HTMLFormElement>("[aria-label='Customer search']")!.requestSubmit();
  await screen.findByRole("link", { name: /Atlas Labs/ });
  assert.equal(document.querySelector("[aria-busy='true']"), null);
});
```

- [ ] **Step 2: Run the test and confirm missing UI modules**

Run: `pnpm --filter @cavi-ai/bobby-gauntlet test -- app-browser.test.ts`

Expected: FAIL because the Northstar shell and routes do not exist.

- [ ] **Step 3: Implement router, shell, dashboard, and customer pages**

Implement history-backed links, accessible loading states, search submission, full row replacement after results resolve, customer detail navigation, priority selection, mutation confirmation, and return navigation. Keep `app.ts` limited to dependency composition and route dispatch.

```ts
export function mountNorthstar(root: HTMLElement, api: NorthstarApi): NorthstarApp {
  const router = createRouter(window);
  const render = async (route: Route) => root.replaceChildren(await pageFor(route, api, router));
  router.subscribe(render);
  void render(router.current());
  return { navigate: router.navigate };
}
```

- [ ] **Step 4: Add the visual system and production bundle wiring**

Define color, spacing, radius, typography, elevation, focus, skeleton, toast, modal, table, form, and responsive navigation tokens in `styles.css`. Bundle it with esbuild's CSS loader and link the emitted stylesheet from `index.html`. Verify layouts at 390px and 1440px without horizontal overflow.

- [ ] **Step 5: Run browser tests, typecheck, and build**

Run: `pnpm --filter @cavi-ai/bobby-gauntlet test && pnpm --filter @cavi-ai/bobby-gauntlet build`

Expected: PASS and `dist/app.js`, `dist/app.css`, and `dist/index.html` exist.

- [ ] **Step 6: Commit the modern shell and customer flow**

```bash
git add packages/bobby-gauntlet
git commit -m "feat(gauntlet): build Northstar customer workspace"
```

### Task 3: Onboarding, documents, integrations, and reports pages

**Files:**
- Create: `packages/bobby-gauntlet/src/pages/onboarding.ts`
- Create: `packages/bobby-gauntlet/src/pages/documents.ts`
- Create: `packages/bobby-gauntlet/src/pages/integrations.ts`
- Create: `packages/bobby-gauntlet/src/pages/reports.ts`
- Modify: `packages/bobby-gauntlet/src/app.ts`
- Modify: `packages/bobby-gauntlet/src/styles.css`
- Modify: `packages/bobby-gauntlet/test/app-browser.test.ts`

**Interfaces:**
- Consumes: `NorthstarApi`, shared components, and router.
- Produces: routes `/onboarding`, `/customers/:id/documents`, `/integrations`, `/reports`, and `/reports/:id`.

- [ ] **Step 1: Write failing page behavior tests**

```ts
test("server field rejection preserves accepted onboarding values", async () => {
  const page = await renderOnboarding(rejectPostalOnceApi());
  await completeIdentityAndOrganization(page, { postalCode: "02110" });
  await submitReview(page);
  assert.equal(input(page, "Company name").value, "Atlas Labs");
  assert.match(alert(page).textContent ?? "", /Use 10001/);
});

test("authorization popup refreshes connected identity", async () => {
  const opened = captureWindowOpen();
  const page = await renderIntegrations(authorizedApi());
  click(page, "Connect Ledger Cloud");
  assert.match(opened.url, /\/authorize/);
  window.dispatchEvent(new MessageEvent("message", { data: { type: "northstar.authorization.complete" } }));
  await findText(page, "Connected as finance@atlas.example");
});
```

- [ ] **Step 2: Run focused browser tests and confirm route failures**

Run: `pnpm --filter @cavi-ai/bobby-gauntlet test -- app-browser.test.ts`

Expected: FAIL because the four page modules are absent.

- [ ] **Step 3: Implement multi-step onboarding with structured server errors**

Use labelled controls, a progress list, plan-dependent billing fields, review output, server field-error mapping, focus transfer to the error summary, value preservation, and confirmation route navigation.

- [ ] **Step 4: Implement upload and iframe preview**

Use a labelled file input, real `FormData`, progress/status announcements, document metadata, and a same-origin iframe preview URL returned by the API. The iframe contains a normal confirmation form posted to the scenario server.

- [ ] **Step 5: Implement popup authorization and obstruction behavior**

Open the returned authorization URL with a named popup, accept only same-origin completion messages, refetch integration state, show a consent/notification obstruction, and provide a visible labelled dismiss control.

- [ ] **Step 6: Implement report generation and download**

Submit report options, render observable pending/complete states, poll with a bounded application deadline, and render an ordinary download link when complete.

- [ ] **Step 7: Run package gates and commit**

Run: `pnpm --filter @cavi-ai/bobby-gauntlet test && pnpm --filter @cavi-ai/bobby-gauntlet build`

Expected: PASS.

```bash
git add packages/bobby-gauntlet
git commit -m "feat(gauntlet): add Northstar operational workflows"
```

### Task 4: Seeded scenario server and durable state

**Files:**
- Create: `crates/runtime-tests/tests/modern_gauntlet/scenario.rs`
- Create: `crates/runtime-tests/tests/modern_gauntlet/mod.rs`
- Create: `crates/runtime-tests/tests/fixtures/approved-upload.txt`
- Modify: `crates/runtime-tests/Cargo.toml`

**Interfaces:**
- Produces: `ScenarioServer::start(ScenarioConfig) -> TestResult<ScenarioServer>`.
- Produces: `ScenarioServer::url() -> Url`, `state() -> ScenarioSnapshot`, `request_log() -> Vec<RequestRecord>`, and `shutdown()`.
- Produces: `ScenarioConfig { seed: String, reject_postal_once: bool, report_interrupt: bool }`.
- Produces: run-scoped endpoints matching `NorthstarApi`.

- [ ] **Step 1: Write failing scenario state tests in `scenario.rs`**

```rust
#[tokio::test]
async fn priority_mutation_is_run_scoped_and_counted_once() {
    let server = ScenarioServer::start(ScenarioConfig::seeded("customer-update")).await.unwrap();
    let client = reqwest::Client::new();
    client.patch(server.url().join("api/customers/cus_atlas/priority").unwrap())
        .header("x-northstar-run", server.run_id())
        .json(&serde_json::json!({"priority":"high"})).send().await.unwrap().error_for_status().unwrap();
    let state = server.state().await;
    assert_eq!(state.customers["cus_atlas"].priority, "high");
    assert_eq!(state.effects.priority_updates, 1);
}
```

- [ ] **Step 2: Run the scenario unit test and confirm missing fixture types**

Run: `cargo test -p runtime-tests --test modern_gauntlet_e2e scenario::tests --locked`

Expected: FAIL because the scenario module is not implemented.

- [ ] **Step 3: Implement static serving and run-scoped API state**

Use Axum state containing `Arc<Mutex<BTreeMap<RunId, RunState>>>`. Reject missing/unknown run headers with structured JSON. Serve only canonicalized files beneath `packages/bobby-gauntlet/dist`. Implement dashboard, customers, priority, onboarding, upload, preview confirmation, authorization, and reports endpoints.

- [ ] **Step 4: Implement deterministic delays, rejection, artifacts, and request log**

Derive identifiers and expected values from the supplied seed. Count durable effects separately from requests. Reject the first configured postal submission, make report generation idempotent by operation key, retain uploaded bytes/digest, and record sanitized request metadata.

- [ ] **Step 5: Run scenario tests and commit**

Run: `cargo test -p runtime-tests --test modern_gauntlet_e2e scenario::tests --locked`

Expected: PASS.

```bash
git add crates/runtime-tests/Cargo.toml crates/runtime-tests/tests/modern_gauntlet crates/runtime-tests/tests/fixtures/approved-upload.txt Cargo.lock
git commit -m "test(runtime): add deterministic Northstar scenario server"
```

### Task 5: Runtime driver and failure evidence

**Files:**
- Create: `crates/runtime-tests/tests/modern_gauntlet/driver.rs`
- Create: `crates/runtime-tests/tests/modern_gauntlet/evidence.rs`
- Modify: `crates/runtime-tests/tests/modern_gauntlet/mod.rs`

**Interfaces:**
- Produces: `ModernRuntime::launch(&ScenarioServer, Journey) -> TestResult<ModernRuntime>`.
- Produces: `navigate`, `inspect`, `click`, `fill`, `complete_form`, `upload`, `wait_for`, `click_popup`, `click_download`, and `restart_from_journal` wrappers over public commands.
- Produces: `EvidenceBundle::capture(...)`, `assert_visible`, `assert_effect_count`, `assert_file_digest`, and `assert_journal_terminal_once`.

- [ ] **Step 1: Write failing driver lifecycle tests**

```rust
#[tokio::test]
async fn missing_bundle_is_a_typed_startup_failure() {
    let error = ModernRuntime::launch_at(Path::new("missing-dist"), Journey::CustomerUpdate).await.unwrap_err();
    assert!(matches!(error, HarnessError::MissingBundle { .. }));
}
```

- [ ] **Step 2: Run focused tests and confirm missing driver failure**

Run: `cargo test -p runtime-tests --test modern_gauntlet_e2e driver::tests --locked`

Expected: FAIL because driver and evidence types do not exist.

- [ ] **Step 3: Extract installed-Chromium setup into `ModernRuntime`**

Reuse existing `runtime-tests` launch and worker configuration. Use unique temporary directories, explicit readiness, public `RuntimeCommand` envelopes, and bounded command deadlines. Do not copy championship scorecard or station-controller logic.

- [ ] **Step 4: Implement evidence bundle capture**

On assertion or command error, capture a final screenshot, inspection/DOM evidence, journal copy, run manifest, scenario request log, and server snapshot beneath `target/modern-gauntlet-artifacts/<journey>/<run-id>/`. Record unavailable console/network channels explicitly rather than claiming collection.

- [ ] **Step 5: Implement durable and journal assertions**

Assert effect counts, uploaded/downloaded SHA-256, terminal journal count, retry bounds, and recovery lineage with error messages naming the journey and artifact directory.

- [ ] **Step 6: Run driver tests and commit**

Run: `cargo test -p runtime-tests --test modern_gauntlet_e2e driver::tests evidence::tests --locked`

Expected: PASS.

```bash
git add crates/runtime-tests/tests/modern_gauntlet
git commit -m "test(runtime): add modern browser journey driver"
```

### Task 6: Five mandatory runtime journeys

**Files:**
- Create: `crates/runtime-tests/tests/modern_gauntlet_e2e.rs`
- Modify: `crates/runtime-tests/tests/modern_gauntlet/mod.rs`

**Interfaces:**
- Consumes: scenario, driver, and evidence interfaces from Tasks 4 and 5.
- Produces: exactly the five test names fixed in the design spec and `REQUIRED_JOURNEYS: [&str; 5]`.

- [ ] **Step 1: Add the release-suite manifest test and five failing test shells**

```rust
const REQUIRED_JOURNEYS: [&str; 5] = [
    "customer_discovery_and_update_is_durable",
    "validated_onboarding_preserves_accepted_values",
    "document_upload_preview_and_confirmation_are_durable",
    "popup_authorization_survives_obstruction",
    "interrupted_report_recovers_once_and_downloads",
];

#[test]
fn release_suite_names_are_stable() {
    assert_eq!(REQUIRED_JOURNEYS.len(), 5);
    assert_eq!(REQUIRED_JOURNEYS.iter().copied().collect::<BTreeSet<_>>().len(), 5);
}
```

The five live tests have no `#[ignore]` attribute. Environment validation occurs inside the harness and returns a hard error when Chromium or the bundle is absent.

- [ ] **Step 2: Run `--list` and prove all five names are discovered**

Run: `cargo test -p runtime-tests --test modern_gauntlet_e2e --locked -- --list`

Expected: output contains each required name exactly once plus the manifest test.

- [ ] **Step 3: Implement customer and onboarding journeys**

Use public navigation, inspect, click, fill/form, and wait commands. Assert route/content, one priority mutation, preserved onboarding values, one onboarding record, and terminal journal evidence.

- [ ] **Step 4: Implement document and authorization journeys**

Upload the approved fixture through the public upload command, act inside the preview frame, complete authorization through the popup command, dismiss the visible obstruction, and assert bytes, metadata, confirmation, grant, and popup cleanup.

- [ ] **Step 5: Implement interrupted report recovery journey**

Begin report generation, prove the durable executing boundary from the journal, terminate only the owned runtime process, rebuild from the same journal/state directory, wait for completion, download, and assert one effect plus expected filename/media type/digest.

- [ ] **Step 6: Run the production bundle and all five live journeys**

Run: `pnpm --filter @cavi-ai/bobby-gauntlet build && cargo test -p runtime-tests --test modern_gauntlet_e2e --locked -- --test-threads=1`

Expected: five journeys and manifest test PASS; no ignored tests.

- [ ] **Step 7: Commit the release journeys**

```bash
git add crates/runtime-tests/tests/modern_gauntlet_e2e.rs crates/runtime-tests/tests/modern_gauntlet
git commit -m "test(runtime): add five Northstar browser journeys"
```

### Task 7: CI release gate and diagnostics

**Files:**
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: production bundle and `modern_gauntlet_e2e` test binary.
- Produces: blocking Chrome CI execution and failure artifact upload.

- [ ] **Step 1: Add a local structural test for CI selection**

Add a Rust or Node test that reads `.github/workflows/ci.yml` and asserts it contains the gauntlet build command, `--test modern_gauntlet_e2e`, and does not place `--ignored` after that test selection.

- [ ] **Step 2: Run the structural test and confirm it fails against current CI**

Run: the focused test command selected in Step 1.

Expected: FAIL because current Chrome CI neither builds nor selects the new suite.

- [ ] **Step 3: Wire the Chrome job**

After locked dependency installation, run `pnpm --filter @cavi-ai/bobby-gauntlet build`. Invoke `cargo test -p runtime-tests --locked --test modern_gauntlet_e2e -- --test-threads=1` without `--ignored`. Keep Chromium location and smoke diagnostics explicit.

- [ ] **Step 4: Upload failure artifacts**

Add `actions/upload-artifact` guarded by `if: failure()` for `target/modern-gauntlet-artifacts/**`, with a bounded retention period and `if-no-files-found: warn` so the primary failure remains visible.

- [ ] **Step 5: Run YAML/structural checks and commit**

Run: the focused structural test plus `git diff --check`.

Expected: PASS.

```bash
git add .github/workflows/ci.yml crates/runtime-tests/tests
git commit -m "ci: gate releases on modern browser journeys"
```

### Task 8: Remove the synthetic championship and document the release suite

**Files:**
- Delete: `crates/runtime-tests/tests/bobby_skills_gauntlet.rs`
- Delete: `packages/bobby-gauntlet/src/controller.ts`
- Delete: `packages/bobby-gauntlet/src/manifest.ts`
- Delete: `packages/bobby-gauntlet/src/scorecard.ts`
- Delete: `packages/bobby-gauntlet/src/station.ts`
- Delete: `packages/bobby-gauntlet/src/stations/**`
- Delete or rewrite: obsolete station/controller tests under `packages/bobby-gauntlet/test/**`
- Modify: `docs/bobby-browser/source/pages/guides/gauntlet.md`
- Modify: `docs/bobby-browser/v0.6.0/guides/gauntlet.md`

**Interfaces:**
- Consumes: passing five-journey suite and package tests.
- Produces: one authoritative modern release contract and reproducible documentation.

- [ ] **Step 1: Run both old characterization and new suite before deletion**

Run: `pnpm --filter @cavi-ai/bobby-gauntlet test && pnpm --filter @cavi-ai/bobby-gauntlet build && cargo test -p runtime-tests --test modern_gauntlet_e2e --locked -- --test-threads=1`

Expected: PASS. Record capability parity in the commit notes only as facts supported by test names.

- [ ] **Step 2: Remove controller, scorecard, stations, and monolithic championship test**

Delete only modules no longer imported by the modern application or retained unit tests. Remove obsolete build directory-copy commands from `package.json`.

- [ ] **Step 3: Rewrite the gauntlet guide**

Document Northstar Ops, the five exact journey names, locked build/test commands, Chromium prerequisite, hard-failure behavior for missing prerequisites, artifact directory, and the distinction between package tests and release E2E.

- [ ] **Step 4: Run dead-reference and package checks**

Run: `rg -n "championship-scorecard|station-scorecard|bobby_skills_gauntlet|GauntletController" packages/bobby-gauntlet crates/runtime-tests docs/bobby-browser/source/pages/guides/gauntlet.md`

Expected: no matches.

Run: `pnpm --filter @cavi-ai/bobby-gauntlet test && pnpm --filter @cavi-ai/bobby-gauntlet build && cargo test -p runtime-tests --locked`

Expected: PASS.

- [ ] **Step 5: Commit migration cleanup and documentation**

```bash
git add packages/bobby-gauntlet crates/runtime-tests docs/bobby-browser/source/pages/guides/gauntlet.md docs/bobby-browser/v0.6.0/guides/gauntlet.md
git commit -m "docs: make Northstar journeys the gauntlet contract"
```

### Task 9: Final verification and review

**Files:**
- Verify all files changed by Tasks 1–8.

**Interfaces:**
- Produces: evidence that current branch behavior matches the approved spec.

- [ ] **Step 1: Run formatting and static checks**

Run: `cargo fmt --all -- --check && pnpm --filter @cavi-ai/bobby-gauntlet typecheck && git diff --check main...HEAD`

Expected: PASS.

- [ ] **Step 2: Run package and non-live runtime tests**

Run: `pnpm --filter @cavi-ai/bobby-gauntlet test && pnpm --filter @cavi-ai/bobby-gauntlet build && cargo test -p runtime-tests --locked`

Expected: PASS.

- [ ] **Step 3: Run the five-journey installed-Chromium gate**

Run: `cargo test -p runtime-tests --test modern_gauntlet_e2e --locked -- --test-threads=1`

Expected: manifest test and all five named journeys PASS with zero ignored.

- [ ] **Step 4: Inspect the complete diff and commit graph**

Run: `git status --short && git log --oneline main..HEAD && git diff --stat main...HEAD && git diff --check main...HEAD`

Expected: clean worktree, only intended commits/files, and no whitespace errors.

- [ ] **Step 5: Request code review and resolve actionable findings**

Use the repository review workflow against `main...HEAD`. Re-run every affected focused gate after fixes, followed by the complete verification commands above.

---

## Plan Self-Review Results

- Spec coverage: application realism, five journeys, public runtime boundary, durable evidence, artifacts, recovery, CI hard gate, legacy removal, and documentation each map to explicit tasks.
- Scope: the application and harness are coupled parts of one release proof and produce one independently testable result; splitting them would leave either a site without proof or proof without a target.
- Type consistency: `NorthstarApi`, `ScenarioServer`, `ModernRuntime`, `EvidenceBundle`, `ScenarioConfig`, and the five journey names are defined once and consumed consistently.
- Placeholder scan: no deferred requirements or unnamed implementation steps remain.
