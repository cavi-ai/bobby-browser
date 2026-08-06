# Upstream Regression Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Repair six verified persistence, retention, compaction, and native-host safety regressions and permanently cover their adversarial cases.

**Architecture:** Keep the existing JSON context store, job journal, and native-host installer boundaries. Replace hand-written identity and lock semantics with maintained PSL and OS advisory-lock primitives, wire retention into runtime construction, make compaction explicitly newest-first, and make installation ownership and rollback exact and observable.

**Tech Stack:** Rust 2021, Tokio, serde, `psl` 2.1.223, standard-library file locking, Cargo workspace tests.

## Global Constraints

- Work only in the isolated `audit/upstream-recheck-20260805` worktree.
- Every production change follows a witnessed red-green test cycle.
- Context records never persist typed values, credentials, full page text, screenshots, journal IDs, or exact timestamps.
- Pending and running jobs are never removed by compaction.
- Operator-owned native-host destinations are never overwritten.
- No push or pull request is part of this plan.

---

### Task 1: Collision-safe context identity

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `crates/context-store/Cargo.toml`
- Modify: `crates/context-store/src/sitekey.rs`
- Modify: `crates/context-store/src/lib.rs`
- Modify: `crates/context-store/tests/store.rs`

**Interfaces:**
- Consumes: `url::Url`, `psl::domain`, UTF-8 profile IDs.
- Produces: `site_key(&str) -> Option<String>` and an injective private `encode_component(&str) -> String`.

- [ ] **Step 1: Add failing tenant-isolation tests**

Extend `site_key_table` with exact expectations for `alice.github.io`,
`bob.github.io`, `one.pages.dev`, and `two.pages.dev`. Add a store test that
opens `a/b` and `a_b`, writes different records, reopens each, and asserts no
cross-profile record appears.

- [ ] **Step 2: Verify RED**

Run: `RUSTC_WRAPPER= cargo test -p context-store site_key -- --nocapture`

Run: `RUSTC_WRAPPER= cargo test -p context-store profiles_with_colliding_sanitized_names_are_isolated -- --nocapture`

Expected: tenant keys collapse to `github.io`/`pages.dev`, or the profile test encounters the same directory/lock.

- [ ] **Step 3: Implement maintained PSL and injective encoding**

Add workspace dependency `psl = "2.1.223"`. Derive the registrable domain
through `psl::domain(host.as_bytes())`, falling back only for IP literals and
single-label hosts. Replace lossy underscore sanitization for profile and site
filenames with lowercase hexadecimal encoding of UTF-8 bytes.

- [ ] **Step 4: Verify GREEN**

Run: `RUSTC_WRAPPER= cargo test -p context-store -- --nocapture`

- [ ] **Step 5: Commit the slice**

Commit message: `fix(context): isolate persisted site and profile identity`

### Task 2: Crash-safe locking and enforced TTL

**Files:**
- Modify: `crates/context-store/src/lib.rs`
- Modify: `crates/context-store/tests/store.rs`
- Modify: `crates/sdk-core/src/lib.rs`
- Modify: `crates/sdk-core/tests/interface_api.rs`

**Interfaces:**
- Consumes: `ContextConfig::ttl_days`, `day_since_epoch(Utc::now())`.
- Produces: `ContextStore::open_with_ttl(root, profile_id, ttl_days, today)` for deterministic construction and tests; `ContextStore::open` remains the no-sweep compatibility constructor.

- [ ] **Step 1: Add failing stale-lock and runtime-TTL tests**

Create a regular `.context-store.lock` before opening and assert the store can
claim its advisory lock. Keep the existing simultaneous-writer test. Add an
SDK construction test that persists an expired record, builds with
`context.ttl_days = 90`, drops the runtime, reopens the store, and asserts the
record was swept.

- [ ] **Step 2: Verify RED**

Run: `RUSTC_WRAPPER= cargo test -p context-store stale_lockfile_does_not_block_recovery -- --nocapture`

Run: `RUSTC_WRAPPER= cargo test -p sdk-core context_ttl_is_applied_during_runtime_build -- --nocapture`

Expected: the first returns `AlreadyLocked`; the second retains the expired record.

- [ ] **Step 3: Implement advisory locking and TTL construction**

Open the lockfile without truncation, reject symlinks/non-files, and call
`try_lock()` on its descriptor. Map `WouldBlock` to `AlreadyLocked`; leave the
file in place on drop and let descriptor release unlock it. Add deterministic
`open_with_ttl`, invoke `sweep`, and have `RuntimeService::build_with_context_promotion`
use `config.context.ttl_days` and the current UTC day.

- [ ] **Step 4: Verify GREEN**

Run: `RUSTC_WRAPPER= cargo test -p context-store -- --nocapture`

Run: `RUSTC_WRAPPER= cargo test -p sdk-core --test interface_api -- --nocapture`

- [ ] **Step 5: Commit the slice**

Commit message: `fix(context): recover locks and enforce retention`

### Task 3: Newest-first scheduler compaction

**Files:**
- Modify: `crates/task-scheduler/src/store.rs`
- Modify: `crates/task-scheduler/tests/integration.rs`

**Interfaces:**
- Consumes: terminal `completed_at` with `created_at` fallback.
- Produces: compacted journal containing all active jobs and the newest 1,024 terminal jobs.

- [ ] **Step 1: Add an exact-survivor compaction test**

Write more than 4,096 journal records with unique timestamps, at least one
pending job, and more than 1,024 terminal jobs. Reopen after compaction and
assert the pending ID and newest terminal IDs exist while the oldest terminal
IDs do not.

- [ ] **Step 2: Verify RED**

Run: `RUSTC_WRAPPER= cargo test -p task-scheduler oversized_journal_retains_newest_terminal_jobs -- --nocapture`

Expected: newest terminal IDs are missing and oldest IDs remain.

- [ ] **Step 3: Implement newest-first retention**

Partition active and terminal jobs. Sort terminal jobs descending by
`completed_at.unwrap_or(created_at)`, truncate to 1,024, then serialize active
plus retained terminal jobs in deterministic ascending time order.

- [ ] **Step 4: Verify GREEN**

Run: `RUSTC_WRAPPER= cargo test -p task-scheduler -- --nocapture`

- [ ] **Step 5: Commit the slice**

Commit message: `fix(scheduler): retain newest compacted job results`

### Task 4: Exact native-host wrapper ownership

**Files:**
- Modify: `crates/cli/src/main.rs`

**Interfaces:**
- Consumes: wrapper bytes generated by `install_native_host` and `shell_quote`.
- Produces: `is_bobby_managed_wrapper(&[u8]) -> bool` accepting only the generated two-line grammar.

- [ ] **Step 1: Add failing ownership tests**

Add table-driven unit cases accepting generated wrappers with quoted paths and
rejecting a comment-only marker, extra commands, missing `exec`, trailing
content, malformed quoting, non-UTF-8, and a different subcommand.

- [ ] **Step 2: Verify RED**

Run: `RUSTC_WRAPPER= cargo test -p bobby-browser native_host_wrapper_ownership_requires_exact_generated_grammar -- --nocapture`

Expected: the comment and extra-command lookalikes are incorrectly accepted.

- [ ] **Step 3: Implement exact parser**

Require exactly two newline-terminated lines, require the second line to start
with `exec `, parse shell words using a small single-quote parser compatible
with `shell_quote`, and require exactly four tokens: executable,
`firefox-native-host`, `--descriptor`, descriptor.

- [ ] **Step 4: Verify GREEN**

Run: `RUSTC_WRAPPER= cargo test -p bobby-browser native_host -- --nocapture`

- [ ] **Step 5: Commit the slice**

Commit message: `fix(cli): require exact native-host wrapper ownership`

### Task 5: Lossless and observable native-host rollback

**Files:**
- Modify: `crates/cli/src/main.rs`

**Interfaces:**
- Produces: `InstallFileOutcome::rollback(self, path) -> io::Result<()>` and replacement metadata containing original bytes and original Unix mode.

- [ ] **Step 1: Add failing rollback tests**

Add a Unix test that replaces a managed wrapper with mode `0750`, forces the
manifest install to fail after preflight, and asserts the original bytes and
`0750` mode return. Add a deterministic injected-write-failure test asserting
the returned message contains both the manifest failure and rollback failure.

- [ ] **Step 2: Verify RED**

Run: `RUSTC_WRAPPER= cargo test -p bobby-browser native_host_replacement_rollback -- --nocapture`

Expected: restored mode is `0700`, or rollback failure is absent from the returned error.

- [ ] **Step 3: Implement rollback metadata and error propagation**

Capture original permissions before replacement. Return rollback results from
both created and replaced outcomes. When manifest installation fails, attempt
rollback and return the primary error if rollback succeeds; otherwise return
an error containing both causes. Use a test-only write hook only at the atomic
write boundary so production behavior remains unchanged.

- [ ] **Step 4: Verify GREEN**

Run: `RUSTC_WRAPPER= cargo test -p bobby-browser native_host -- --nocapture`

- [ ] **Step 5: Commit the slice**

Commit message: `fix(cli): preserve and report native-host rollback`

### Task 6: Final regression and repository gates

**Files:**
- Modify only files required by formatter output.

**Interfaces:**
- Consumes: all prior task outputs.
- Produces: a clean, reviewable branch with current-head verification evidence.

- [ ] **Step 1: Run formatting and diff checks**

Run: `cargo fmt --all -- --check`

Run: `git diff --check`

- [ ] **Step 2: Run focused regression suites**

Run: `RUSTC_WRAPPER= cargo test -p context-store -- --nocapture`

Run: `RUSTC_WRAPPER= cargo test -p sdk-core --test interface_api -- --nocapture`

Run: `RUSTC_WRAPPER= cargo test -p task-scheduler -- --nocapture`

Run: `RUSTC_WRAPPER= cargo test -p bobby-browser native_host -- --nocapture`

- [ ] **Step 3: Run workspace build, tests, and lint**

Run: `RUSTC_WRAPPER= cargo test --workspace`

Run: `RUSTC_WRAPPER= cargo clippy --workspace --all-targets -- -D warnings`

- [ ] **Step 4: Rebuild and verify the review graph**

Run the code-review-graph full update against the current worktree and confirm
its `built_at_sha` matches `HEAD` after commits.

- [ ] **Step 5: Audit the final branch**

Run: `git status --short --branch`

Run: `git log --oneline origin/main..HEAD`

Run: `git diff --stat origin/main..HEAD`

Confirm only the design, plan, implementation, and regression tests are present.
