# Browser Runtime Vertical Slice Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the in-memory navigation placeholder with a durable command pipeline that leases one isolated Chromium process per session and proves real navigate, inspect, click, and type operations end to end.

**Architecture:** External requests enter through the existing SDK and broker, become typed command envelopes, are journaled before execution, and are dispatched by `page-runtime` through a driver-neutral `BrowserWorker` interface. `worker-pool` implements that interface with Chromiumoxide 0.9.1, a private profile directory per session, and one Chromium process per active session; tests use an in-process fake driver or a deterministic local web fixture.

**Tech Stack:** Rust 2021, Tokio, Axum 0.8, Serde, UUID, Chromiumoxide 0.9.1, Futures 0.3, async-trait 0.1, tempfile 3, JSON Lines durable journal, Cargo workspace tests.

## Global Constraints

- Chromium is the only production browser engine in this slice, but all browser behavior must remain behind `BrowserWorker` and `WorkerFactory` interfaces.
- The calling agent owns goals and strategic decisions; this slice implements stable primitives and evidence, not model-specific planning.
- Every active session owns one dedicated Chromium process and private profile directory.
- Every command is durably prepared before browser execution and durably completed or failed before a terminal outcome is returned.
- A driver response alone is not success; navigation, click, and typing commands must verify observable postconditions.
- Public web content cannot access other session profiles, arbitrary host files, or daemon state.
- The scheduler target remains eight active workflows and approximately 32 warm or resumable sessions; this slice enforces an eight-process active limit and bounded acquisition.
- Smoke tests are preliminary only. Completion requires a live Chromium integration test against the deterministic fixture.
- Do not implement adaptive HTTP, full checkpoint recovery, uploads/downloads, MCP transport, CDP compatibility routing, or intent planning in this plan; each is a subsequent independently testable plan.

## File Structure

- `crates/types/src/ids.rs`: stable identifiers for sessions, pages, commands, workflows, attempts, and evidence.
- `crates/types/src/commands.rs`: command envelope and primitive command payloads.
- `crates/types/src/outcomes.rs`: typed outcomes, errors, evidence, and journal phases.
- `crates/types/src/state.rs`: session, page, and runtime state models.
- `crates/types/src/lib.rs`: re-exports only.
- `crates/workflow-journal/src/lib.rs`: journal trait and JSONL implementation.
- `crates/worker-pool/src/lib.rs`: driver-neutral worker traits, bounded lease manager, and exports.
- `crates/worker-pool/src/chromium.rs`: Chromiumoxide process lifecycle and primitive execution.
- `crates/page-runtime/src/lib.rs`: page registry and durable command executor.
- `crates/session-manager/src/lib.rs`: session creation and worker lease ownership.
- `crates/sdk-core/src/lib.rs`: application service composition and command submission.
- `crates/broker/src/lib.rs`: HTTP translation and status mapping.
- `crates/test-site/src/lib.rs`: deterministic dynamic form fixture.
- `crates/runtime-tests/tests/browser_vertical_slice.rs`: live Chromium proof.

---

### Task 1: Stable Command, Outcome, and Evidence Contracts

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/types/Cargo.toml`
- Create: `crates/types/src/ids.rs`
- Create: `crates/types/src/commands.rs`
- Create: `crates/types/src/outcomes.rs`
- Create: `crates/types/src/state.rs`
- Modify: `crates/types/src/lib.rs`
- Test: `crates/types/tests/contracts.rs`

**Interfaces:**
- Consumes: existing `SessionId`, `PageId`, `SessionState`, `PageState`, `NavigationRequest`, and `RuntimeError` concepts.
- Produces: `CommandEnvelope`, `PrimitiveCommand`, `CommandOutcome`, `CommandError`, `Evidence`, `CommandPhase`, and all identifier types used by every later task, including `WorkerId`.

- [ ] **Step 1: Write serialization and classification tests**

Create `crates/types/tests/contracts.rs` with tests that construct a fixed-ID `CommandEnvelope`, serialize it, and assert `schemaVersion`, `commandId`, `sessionId`, `pageId`, and the tagged `navigate` payload. Add a second test asserting `PrimitiveCommand::Inspect` is replayable, `TypeText` is reconciliable, and `Click { boundary: true }` is a boundary action.

```rust
use types::{CommandClass, InspectCommand, PrimitiveCommand, TypeTextCommand};

#[test]
fn commands_expose_recovery_class() {
    assert_eq!(
        PrimitiveCommand::Inspect(InspectCommand::default()).class(),
        CommandClass::Replayable
    );
    assert_eq!(
        PrimitiveCommand::TypeText(TypeTextCommand {
            selector: "#name".into(), value: "Ada".into(), clear_first: true,
        }).class(),
        CommandClass::Reconciliable
    );
}
```

- [ ] **Step 2: Run the contract test and verify it fails**

Run: `cargo test -p types --test contracts`

Expected: compilation fails because the command contract modules do not exist.

- [ ] **Step 3: Split the types crate into focused modules**

Move existing state types into `state.rs`, put UUID newtypes in `ids.rs`, and define the following exact API in `commands.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandEnvelope {
    pub schema_version: u16,
    pub command_id: CommandId,
    pub workflow_id: WorkflowId,
    pub attempt_id: AttemptId,
    pub session_id: SessionId,
    pub page_id: Option<PageId>,
    pub deadline: DateTime<Utc>,
    pub command: PrimitiveCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "input", rename_all = "camelCase")]
pub enum PrimitiveCommand {
    Navigate(NavigateCommand),
    Inspect(InspectCommand),
    Click(ClickCommand),
    TypeText(TypeTextCommand),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CommandClass { Replayable, Reconciliable, Boundary }

impl PrimitiveCommand {
    pub fn class(&self) -> CommandClass {
        match self {
            Self::Navigate(_) | Self::Inspect(_) => CommandClass::Replayable,
            Self::TypeText(_) => CommandClass::Reconciliable,
            Self::Click(command) if command.boundary => CommandClass::Boundary,
            Self::Click(_) => CommandClass::Reconciliable,
        }
    }
}
```

Define `NavigateCommand { url, wait_until, timeout_ms }`, `InspectCommand { selector: Option<String>, include_html: bool }`, `ClickCommand { selector, boundary, expected_url: Option<String> }`, and `TypeTextCommand { selector, value, clear_first }`. Keep JSON field names camelCase and set `CommandEnvelope::SCHEMA_VERSION` to `1`.

Define `WaitUntil::{Commit, DomContentLoaded, Interactive, NetworkIdle}` and use it as `NavigateCommand.wait_until`. Derive `Default` for `InspectCommand` with `selector = None` and `include_html = false`. Add `WorkerId` as a UUID newtype beside the other identifiers.

In `outcomes.rs`, define:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum CommandOutcome {
    Completed { command_id: CommandId, evidence: Vec<Evidence> },
    RetryableFailure { command_id: CommandId, error: CommandError },
    NeedsReconciliation { command_id: CommandId, error: CommandError, evidence: Vec<Evidence> },
    PolicyDenied { command_id: CommandId, error: CommandError },
    ResourceExhausted { command_id: CommandId, error: CommandError, retry_after_ms: u64 },
    Restarted { command_id: CommandId, prior_attempt_id: AttemptId, attempt_id: AttemptId, reason: String },
    Failed { command_id: CommandId, error: CommandError },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Evidence {
    Navigation { url: String, title: String },
    Inspection { url: String, title: String, text: String, html: Option<String> },
    Element { selector: String, text: Option<String> },
}
```

Define stable `CommandError { code: ErrorCode, message: String, layer: ErrorLayer, retryable: bool }`, `ErrorCode`, `ErrorLayer`, and `CommandPhase::{Accepted, Prepared, Executing, Verifying, Completed, Failed}`. Re-export every public contract from `lib.rs`.

- [ ] **Step 4: Run types tests and workspace checks**

Run: `cargo test -p types && cargo check --workspace`

Expected: all tests pass; downstream crates require only import fixes caused by the module split.

- [ ] **Step 5: Commit the contract boundary**

```bash
git add Cargo.toml crates/types
git commit -m "feat: define browser command contracts"
```

---

### Task 2: Crash-Durable Append-Only Command Journal

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/workflow-journal/Cargo.toml`
- Create: `crates/workflow-journal/src/lib.rs`
- Test: `crates/workflow-journal/tests/jsonl_journal.rs`

**Interfaces:**
- Consumes: `CommandEnvelope`, `CommandId`, `CommandPhase`, `CommandOutcome`.
- Produces: `CommandJournal` trait, `JsonlJournal::open(path)`, `append(record)`, and `history(command_id)`.

- [ ] **Step 1: Write failure-first persistence tests**

Test that `append()` writes `Accepted`, `Prepared`, and `Completed` records; reopen the file and assert `history()` returns the same ordered records. Truncate the final JSON line halfway, reopen, and assert complete earlier records remain readable while the torn tail is ignored and reported by `JournalScan::torn_tail`.

```rust
#[tokio::test]
async fn reopens_committed_history_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let journal = JsonlJournal::open(&path).await.unwrap();
    journal.append(record(CommandPhase::Accepted)).await.unwrap();
    journal.append(record(CommandPhase::Prepared)).await.unwrap();
    drop(journal);

    let reopened = JsonlJournal::open(&path).await.unwrap();
    let scan = reopened.history(command_id()).await.unwrap();
    assert_eq!(scan.records.len(), 2);
    assert!(!scan.torn_tail);
}
```

- [ ] **Step 2: Run the journal test and verify it fails**

Run: `cargo test -p workflow-journal --test jsonl_journal`

Expected: Cargo reports that package `workflow-journal` does not exist.

- [ ] **Step 3: Implement the journal with flush-before-acknowledgement semantics**

Add `workflow-journal` to workspace members. Define:

```rust
#[async_trait]
pub trait CommandJournal: Send + Sync {
    async fn append(&self, record: JournalRecord) -> Result<(), JournalError>;
    async fn history(&self, id: CommandId) -> Result<JournalScan, JournalError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalRecord {
    pub sequence: u64,
    pub recorded_at: DateTime<Utc>,
    pub command_id: CommandId,
    pub phase: CommandPhase,
    pub envelope: Option<CommandEnvelope>,
    pub outcome: Option<CommandOutcome>,
}
```

`JsonlJournal` must hold a Tokio mutex around a `tokio::fs::File` and monotonically increasing sequence. `append()` serializes exactly one record, appends `\n`, calls `flush()`, then `sync_data()`, and only then returns. `open()` scans existing complete lines, restores the next sequence, and tolerates only an incomplete final line. Malformed complete lines return `JournalError::Corrupt { line }`.

- [ ] **Step 4: Prove durability and concurrency behavior**

Add a test spawning 32 concurrent appends and assert sequence values are unique and strictly increasing after reopen.

Run: `cargo test -p workflow-journal`

Expected: all journal tests pass, including torn-tail and concurrent append cases.

- [ ] **Step 5: Commit the durable journal**

```bash
git add Cargo.toml crates/workflow-journal
git commit -m "feat: add durable command journal"
```

---

### Task 3: Driver-Neutral Dedicated Chromium Worker Pool

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/config/src/lib.rs`
- Modify: `crates/worker-pool/Cargo.toml`
- Modify: `crates/worker-pool/src/lib.rs`
- Create: `crates/worker-pool/src/chromium.rs`
- Test: `crates/worker-pool/tests/pool.rs`
- Test: `crates/worker-pool/tests/chromium_worker.rs`

**Interfaces:**
- Consumes: `SessionId`, `PageId`, primitive command payloads, `Evidence`, `CommandError`.
- Produces: `BrowserWorker`, `WorkerFactory`, `WorkerPool::lease(session_id)`, `WorkerLease`, and `ChromiumWorkerFactory`.

- [ ] **Step 1: Write bounded leasing tests with a fake factory**

Test these behaviors: two leases for the same session return the same worker identity; different sessions receive different identities and profile paths; the ninth concurrent distinct session waits when `max_active = 8`; releasing one lease permits the ninth acquisition; a factory launch failure releases its semaphore permit.

```rust
#[tokio::test]
async fn reuses_one_dedicated_worker_per_session() {
    let pool = WorkerPool::new(8, Arc::new(FakeFactory::default()));
    let first = pool.lease(session_id(1)).await.unwrap();
    let second = pool.lease(session_id(1)).await.unwrap();
    assert_eq!(first.worker_id(), second.worker_id());
    assert_eq!(pool.active_workers().await, 1);
}
```

- [ ] **Step 2: Run the pool tests and verify they fail**

Run: `cargo test -p worker-pool --test pool`

Expected: compilation fails because the worker traits and pool do not exist.

- [ ] **Step 3: Define the driver boundary and bounded pool**

Define the exact async interface:

```rust
#[async_trait]
pub trait BrowserWorker: Send + Sync {
    fn worker_id(&self) -> WorkerId;
    fn profile_dir(&self) -> &Path;
    async fn open_page(&self, page_id: PageId) -> Result<(), CommandError>;
    async fn navigate(&self, page_id: &PageId, command: &NavigateCommand) -> Result<Vec<Evidence>, CommandError>;
    async fn inspect(&self, page_id: &PageId, command: &InspectCommand) -> Result<Vec<Evidence>, CommandError>;
    async fn click(&self, page_id: &PageId, command: &ClickCommand) -> Result<Vec<Evidence>, CommandError>;
    async fn type_text(&self, page_id: &PageId, command: &TypeTextCommand) -> Result<Vec<Evidence>, CommandError>;
    async fn close(&self) -> Result<(), CommandError>;
}

#[async_trait]
pub trait WorkerFactory: Send + Sync {
    async fn launch(&self, session_id: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError>;
}
```

Implement `WorkerPool` with a Tokio semaphore and `HashMap<SessionId, Arc<WorkerEntry>>`. A worker entry owns the semaphore permit until explicit `release_session()` or pool shutdown. Never disable the Chromium sandbox in production configuration.

- [ ] **Step 4: Implement the Chromiumoxide adapter**

Add workspace dependencies `chromiumoxide = "0.9.1"`, `futures = "0.3"`, `async-trait = "0.1"`, and `tempfile = "3"`. Extend `AppConfig` with `BrowserConfig { executable: Option<PathBuf>, profiles_dir: PathBuf, headless: bool, max_active: usize }`, defaulting to `./data/profiles`, headless, and eight active processes.

`ChromiumWorkerFactory::launch()` must create `profiles_dir/<session-uuid>`, build Chromiumoxide config with `user_data_dir`, optionally set `chrome_executable`, retain sandbox defaults, launch `Browser`, and spawn a handler-drain task:

```rust
let (browser, mut handler) = Browser::launch(builder.build().map_err(config_error)?).await
    .map_err(launch_error)?;
let handler_task = tokio::spawn(async move {
    while let Some(event) = handler.next().await {
        if event.is_err() { break; }
    }
});
```

Store `Page` handles in a mutex-protected `HashMap<PageId, Page>`. Implement navigation with `page.goto(url)`, then read `page.url()`, `page.get_title()`, and emit `Evidence::Navigation`. Implement inspection with `find_element(selector)` when provided, otherwise `content()`, body text through `evaluate`, current URL, and title. Implement click with `find_element(selector).click()`. Implement typing with `find_element(selector).click().type_str(value)`; when `clear_first` is true, focus and send `ControlOrMeta+A` followed by `Backspace` using a small platform helper covered by a unit test.

- [ ] **Step 5: Add a live, opt-in Chromium worker test**

Start a local Axum fixture, launch one worker with a temporary profile, open a page, navigate, type into `#name`, click `#continue`, and inspect `#result`. Mark only this driver-specific test `#[ignore = "requires installed Chrome or Chromium"]`; the final integration task will run it explicitly.

Run: `cargo test -p worker-pool && cargo test -p worker-pool --test chromium_worker -- --ignored --nocapture`

Expected: unit tests pass; the ignored test passes when Chrome or Chromium is installed and reports the real page title and result text.

- [ ] **Step 6: Commit the isolated worker pool**

```bash
git add Cargo.toml crates/config crates/worker-pool
git commit -m "feat: lease isolated chromium workers"
```

---

### Task 4: Durable Page Command Executor

**Files:**
- Modify: `crates/page-runtime/Cargo.toml`
- Modify: `crates/page-runtime/src/lib.rs`
- Create: `crates/page-runtime/src/executor.rs`
- Test: `crates/page-runtime/tests/executor.rs`

**Interfaces:**
- Consumes: `CommandJournal`, `WorkerPool`, `CommandEnvelope`, and `BrowserWorker`.
- Produces: `PageRuntime::open(session_id)`, `PageRuntime::execute(envelope)`, and `PageRuntime::get(page_id)`.

- [ ] **Step 1: Write lifecycle tests using fake journal and worker**

Assert the executor writes phases in this order: `Accepted`, `Prepared`, `Executing`, `Verifying`, `Completed`. Assert browser execution occurs only after `Prepared` append succeeds. Assert an inspect driver failure returns `RetryableFailure`; assert a boundary click driver failure after execution begins returns `NeedsReconciliation`.

```rust
#[tokio::test]
async fn prepares_durably_before_touching_browser() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let runtime = fixture_runtime(events.clone());
    let outcome = runtime.execute(navigate_envelope()).await;
    assert!(matches!(outcome, CommandOutcome::Completed { .. }));
    assert_eq!(&*events.lock().await, &[
        "journal:accepted", "journal:prepared", "journal:executing",
        "browser:navigate", "journal:verifying", "journal:completed",
    ]);
}
```

Record `Executing` immediately before the browser call. This ordering is a durable lifecycle invariant.

- [ ] **Step 2: Run the executor tests and verify they fail**

Run: `cargo test -p page-runtime --test executor`

Expected: compilation fails because `PageRuntime::execute` and injected dependencies do not exist.

- [ ] **Step 3: Implement dependency-injected runtime construction**

Replace `Default` construction with:

```rust
impl PageRuntime {
    pub fn new(journal: Arc<dyn CommandJournal>, workers: Arc<WorkerPool>) -> Self;
    pub async fn open(&self, session_id: SessionId) -> Result<PageState, RuntimeError>;
    pub async fn execute(&self, envelope: CommandEnvelope) -> CommandOutcome;
}
```

Validate schema version, deadline, session/page association, and URL scheme before journaling. Append lifecycle records around driver dispatch. Verify navigate by matching the final evidence URL, type by inspecting the target value, and click by checking `expected_url` when supplied or target existence after the click. Map failures according to command class and execution phase.

- [ ] **Step 4: Run all page runtime tests**

Run: `cargo test -p page-runtime`

Expected: all page registry, ordering, verification, and failure-classification tests pass.

- [ ] **Step 5: Commit the durable executor**

```bash
git add crates/page-runtime
git commit -m "feat: execute durable page commands"
```

---

### Task 5: Compose Sessions, SDK, and HTTP Command API

**Files:**
- Modify: `crates/session-manager/Cargo.toml`
- Modify: `crates/session-manager/src/lib.rs`
- Modify: `crates/sdk-core/Cargo.toml`
- Modify: `crates/sdk-core/src/lib.rs`
- Modify: `crates/broker/Cargo.toml`
- Modify: `crates/broker/src/lib.rs`
- Modify: `crates/cli/src/main.rs`
- Modify: `config.toml`
- Test: `crates/broker/tests/commands.rs`

**Interfaces:**
- Consumes: `WorkerPool`, `JsonlJournal`, `PageRuntime`, and all command contracts.
- Produces: `RuntimeService::build(config)`, `submit(envelope)`, `POST /commands`, and production startup composition.

- [ ] **Step 1: Write broker API tests with a fake runtime**

Build the router separately from socket binding. Assert `POST /commands` returns 200 for `Completed`, 409 for `NeedsReconciliation`, 429 for `ResourceExhausted`, 403 for `PolicyDenied`, 422 for invalid schema or session/page association, and 500 for terminal internal failure. Assert response bodies serialize the typed outcome without a second ad hoc error schema.

- [ ] **Step 2: Run the broker test and verify it fails**

Run: `cargo test -p broker --test commands`

Expected: compilation fails because router construction and command submission are not injectable.

- [ ] **Step 3: Compose production dependencies explicitly**

Implement:

```rust
impl RuntimeService {
    pub async fn build(config: &AppConfig) -> Result<Self, RuntimeError> {
        let journal = Arc::new(JsonlJournal::open(&config.storage.journal_path).await?);
        let factory = Arc::new(ChromiumWorkerFactory::new(config.browser.clone()));
        let workers = Arc::new(WorkerPool::new(config.browser.max_active, factory));
        let pages = PageRuntime::new(journal, workers.clone());
        Ok(Self { sessions: SessionManager::new(workers), pages })
    }

    pub async fn submit(&self, envelope: CommandEnvelope) -> CommandOutcome {
        self.pages.execute(envelope).await
    }
}
```

Session creation must acquire its dedicated worker before returning success. Session deletion must close and release that worker. Page creation must call `BrowserWorker::open_page` before adding the page ID to session state.

Add `StorageConfig { journal_path: PathBuf }` and the browser configuration fields to `config.toml`. Make CLI startup return configuration, browser discovery, journal-open, and bind failures instead of panicking.

- [ ] **Step 4: Add typed HTTP command routing**

Expose `pub fn router(state: AppState) -> Router`. Add `POST /commands` accepting `Json<CommandEnvelope>`, calling `RuntimeService::submit`, and mapping only the HTTP status while returning `Json<CommandOutcome>` unchanged.

- [ ] **Step 5: Run API and workspace tests**

Run: `cargo test -p broker && cargo test --workspace`

Expected: all contract, journal, pool, executor, session, SDK, and broker tests pass; only explicitly ignored live Chromium tests remain skipped.

- [ ] **Step 6: Commit production composition**

```bash
git add crates/session-manager crates/sdk-core crates/broker crates/cli config.toml
git commit -m "feat: expose durable browser commands"
```

---

### Task 6: Deterministic Live-Browser Vertical-Slice Proof

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/test-site/Cargo.toml`
- Create: `crates/test-site/src/lib.rs`
- Create: `crates/runtime-tests/Cargo.toml`
- Create: `crates/runtime-tests/tests/browser_vertical_slice.rs`
- Modify: `scripts/dev/smoke.sh`
- Modify: `README.md`

**Interfaces:**
- Consumes: production `RuntimeService`, worker pool, broker contracts, and installed Chrome or Chromium.
- Produces: deterministic fixture routes and the live release-gate command for this slice.

- [ ] **Step 1: Create the deterministic workflow fixture**

Implement an Axum app with:

- `GET /` returning a page titled `Runtime Fixture`, an input `#name`, button `#continue`, and JavaScript that waits 50 ms before showing step two.
- Step two creates `#company`, `#submit`, and a popup link.
- Submit changes `history.pushState` to `/complete` and renders `#result` with `Submitted: <name> @ <company>`.
- `GET /healthz` returning 200.

Expose `test_site::spawn() -> FixtureServer` where `FixtureServer::base_url()` returns the bound loopback URL and dropping it aborts the server task.

- [ ] **Step 2: Write the ignored live Chromium integration test**

The test must:

1. Create temporary journal and profile roots.
2. Build the real runtime with `max_active = 8`.
3. Create a session and page.
4. Submit navigate and assert title `Runtime Fixture` in evidence.
5. Type `Ada` into `#name` and verify its value.
6. Click `#continue`, wait through repeated inspect commands until `#company` exists, and type `Analytical Engines`.
7. Click boundary action `#submit` with `expected_url = <base>/complete`.
8. Inspect `#result` and assert `Submitted: Ada @ Analytical Engines`.
9. Read the journal and assert every command has `Prepared` before `Executing` and one terminal phase.
10. Assert the session profile directory is distinct from a second session's profile directory.

Mark the test `#[ignore = "requires installed Chrome or Chromium"]` so ordinary unit tests remain portable.

- [ ] **Step 3: Run the live proof and correct real-driver discrepancies**

Run: `cargo test -p runtime-tests --test browser_vertical_slice -- --ignored --nocapture`

Expected: one live Chromium test passes and prints the final verified URL, result text, command count, and distinct profile paths. Fix only adapter behavior exposed by the test; do not weaken assertions or substitute mocked evidence.

- [ ] **Step 4: Add the release-gate script**

Update `scripts/dev/smoke.sh` to remain a health-only preliminary check. Add documented commands in `README.md`:

```bash
cargo test --workspace
cargo test -p runtime-tests --test browser_vertical_slice -- --ignored --nocapture
```

State explicitly that both commands are required for this vertical slice and that the smoke script is not completion proof.

- [ ] **Step 5: Run formatting, linting, tests, and static checks**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p runtime-tests --test browser_vertical_slice -- --ignored --nocapture
rg -n 'no_sandbox|--no-sandbox|unimplemented!|panic!\("not implemented' crates Cargo.toml config.toml
```

Expected: formatting, Clippy, workspace tests, and live Chromium proof pass. The final search finds no sandbox disabling and no unfinished implementation markers in production code.

- [ ] **Step 6: Commit the verified vertical slice**

```bash
git add Cargo.toml crates/test-site crates/runtime-tests scripts/dev/smoke.sh README.md
git commit -m "test: prove live browser command slice"
```

## Follow-On Plans

After this plan is green, write and execute separate plans in this dependency order:

1. Durable browser checkpoints, daemon/process crash injection, reconciliation, and clean restart lineage.
2. Workflow-heavy uploads, downloads, tabs, popups, dialogs, and long-lived session suspension.
3. Semantic target resolution and intent operations over accessibility, DOM, text, geometry, and mutation evidence.
4. Adaptive direct-HTTP execution with cookie/header/cache synchronization and conservative Chromium fallback.
5. MCP stdio/Streamable HTTP and CDP compatibility conformance over the internal protocol.
6. Network, file, secret, execution, and resource policy hardening plus public-site canaries and eight-workflow performance gates.
