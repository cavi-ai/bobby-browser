# Bobby Skill Gauntlet Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Recover the complete Bobby skill runtime and deterministic browser gauntlet onto the canonical public-history baseline, including generated public documentation.

**Architecture:** Port legacy behavior subsystem-by-subsystem, with the current `origin/main` APIs authoritative wherever histories conflict. Tests are restored and observed failing before their production counterparts are ported; the legacy worktree remains a read-only reference.

**Tech Stack:** Rust workspace, Tokio, Serde, TypeScript 7, Node.js 22, pnpm, esbuild, jsdom, generated Markdown documentation.

## Global Constraints

- Target branch is `feat/bobby-skill-gauntlet`, based on `origin/main`.
- Recovery source is the read-only `feat/bobby-skills-gauntlet` branch.
- Preserve current intent, network-wait, shadow-root, observability, lifecycle, multi-principal, release, and documentation behavior.
- Never replace a current overlapping file wholesale; port the skill-specific hunks.
- Tests must demonstrate the missing behavior before production code is added.
- Public documentation is authored under `docs/bobby-browser/source/`; versioned docs are generated only by `pnpm docs:build`.
- Do not change the legacy worktree.

---

### Task 1: Establish the canonical baseline

**Files:**
- Verify: `Cargo.toml`
- Verify: `package.json`
- Verify: `docs/bobby-browser/CONSUMER.md`

**Interfaces:**
- Consumes: canonical `origin/main` repository state.
- Produces: recorded green baseline and installed workspace dependencies.

- [ ] **Step 1: Verify isolation and branch ancestry**

```bash
git status --short --branch
git merge-base --is-ancestor origin/main HEAD
git diff --check
```

- [ ] **Step 2: Install JavaScript dependencies without changing declared versions**

```bash
pnpm install --frozen-lockfile
```

- [ ] **Step 3: Run the canonical baseline gates**

```bash
cargo test --workspace
pnpm docs:test
pnpm docs:verify
```

- [ ] **Step 4: Record any environment-only failures before continuing**

The baseline may proceed only when ordinary workspace and docs gates pass. Browser-dependent ignored tests are not part of this step.

### Task 2: Recover bounded skill wire contracts

**Files:**
- Create: `crates/types/src/skills.rs`
- Modify: `crates/types/src/lib.rs`
- Modify: `crates/types/src/commands.rs`
- Modify: `crates/types/src/outcomes.rs`
- Modify: `crates/types/src/recovery.rs`
- Test: `crates/types/tests/skill_contracts.rs`
- Test: `crates/types/tests/contracts.rs`
- Test: `crates/types/tests/recovery_contracts.rs`

**Interfaces:**
- Consumes: existing `SessionId`, `WorkflowId`, `AttemptId`, `CommandId`, `CheckpointId`, `CommandClass`.
- Produces: `SkillCommand`, `SkillGhostCommand`, `SkillZigZagZigCommand`, `SkillCapability`, `SkillFailure`, `SkillTactic`, `SkillBrowserEngine`, `SkillProfileRequest`, `SkillProfile`, `SkillDecision`, `SkillOutcome`, `SkillSessionState`, `SkillCommandIdentity`, and recovery receipt fields.

- [ ] **Step 1: Restore contract tests only**

```bash
git show feat/bobby-skills-gauntlet:crates/types/tests/skill_contracts.rs > /tmp/skill_contracts.rs
```

Use `apply_patch` to add the recovered test content and port only the skill-specific assertions from the two existing contract test files.

- [ ] **Step 2: Run the tests and verify RED**

```bash
cargo test -p types --test skill_contracts
```

Expected: compilation fails because the skill contract types are absent.

- [ ] **Step 3: Port the contracts and validation**

Use `feat/bobby-skills-gauntlet:crates/types/src/skills.rs` as the behavioral source. Preserve bounded collections, `deny_unknown_fields`, schema version checks, redacted debug/serialization behavior, and current public recovery fields.

- [ ] **Step 4: Run focused contract tests**

```bash
cargo test -p types --test skill_contracts
cargo test -p types --test contracts
cargo test -p types --test recovery_contracts
```

- [ ] **Step 5: Commit the contract layer**

```bash
git add Cargo.lock crates/types
git commit -m "feat: restore bounded Bobby skill contracts"
```

### Task 3: Recover the skill runtime

**Files:**
- Create: `crates/skill-runtime/Cargo.toml`
- Create: `crates/skill-runtime/src/lib.rs`
- Create: `crates/skill-runtime/src/command.rs`
- Create: `crates/skill-runtime/src/registry.rs`
- Create: `crates/skill-runtime/src/router.rs`
- Create: `crates/skill-runtime/src/state.rs`
- Create: `crates/skill-runtime/src/ghost.rs`
- Create: `crates/skill-runtime/src/zigzagzig.rs`
- Create: `crates/skill-runtime/tests/command.rs`
- Create: `crates/skill-runtime/tests/registry.rs`
- Create: `crates/skill-runtime/tests/router.rs`
- Create: `crates/skill-runtime/tests/state.rs`
- Create: `crates/skill-runtime/tests/ghost.rs`
- Create: `crates/skill-runtime/tests/zigzagzig.rs`
- Modify: `Cargo.toml`

**Interfaces:**
- Consumes: Task 2 skill contracts.
- Produces: `parse_skill_command`, `SkillRegistry`, `SkillCommandRouter`, `SkillStateStore`, `SkillEngineAdapter`, `SkillGhost`, and `SkillZigZagZigController`.

- [ ] **Step 1: Add the crate manifest and recovered tests, excluding production modules**

Register `crates/skill-runtime` in the workspace and add the six legacy test files with imports adapted only for current crate paths.

- [ ] **Step 2: Verify RED**

```bash
cargo test -p skill-runtime
```

Expected: unresolved imports for command parsing, registry, state, Ghost, and ZigZagZig behavior.

- [ ] **Step 3: Port command, registry, router, and state modules**

Preserve exact slash forms `/ghost on|off|status` and `/zigzagzig run|status|stop`; reject unknown commands, duplicate aliases, version conflicts, unauthorized capabilities, duplicate sessions, and stale state transitions.

- [ ] **Step 4: Run the core runtime tests**

```bash
cargo test -p skill-runtime --test command
cargo test -p skill-runtime --test registry
cargo test -p skill-runtime --test router
cargo test -p skill-runtime --test state
```

- [ ] **Step 5: Port SkillGhost and SkillZigZagZig**

Preserve requested-versus-effective profile reporting, required/optional capability handling, restart requirements, frozen session identity, tactic ordering, total deadlines, tactic budgets, issued-decision persistence, and terminal failure classification.

- [ ] **Step 6: Run all skill runtime tests**

```bash
cargo test -p skill-runtime
```

- [ ] **Step 7: Commit the runtime**

```bash
git add Cargo.toml Cargo.lock crates/skill-runtime
git commit -m "feat: restore Bobby skill runtime"
```

### Task 4: Recover worker skill adapters

**Files:**
- Create: `crates/worker-pool/src/skill_adapter.rs`
- Modify: `crates/worker-pool/src/lib.rs`
- Modify: `crates/worker-pool/src/selection.rs`
- Modify: `crates/worker-pool/Cargo.toml`
- Test: `crates/worker-pool/tests/skill_adapter.rs`
- Test: `crates/worker-pool/tests/pool.rs`

**Interfaces:**
- Consumes: `SkillEngineAdapter`, `SkillProfileRequest`, current `EnginePreference`, worker leasing and replacement APIs.
- Produces: Firefox and Chromium capability negotiation, effective profile reporting, and safe engine replacement selection.

- [ ] **Step 1: Port adapter tests before implementation**

Add `skill_adapter.rs` tests and only the skill-specific pool assertions.

- [ ] **Step 2: Verify RED**

```bash
cargo test -p worker-pool --test skill_adapter
```

Expected: missing adapter and skill-aware selection APIs.

- [ ] **Step 3: Implement the adapter against current worker APIs**

Do not restore legacy Chromium code, targeting code, process registry, or network wait implementations. Add only the adapter module, exports, dependencies, and selection extensions required by the tests.

- [ ] **Step 4: Run focused and regression tests**

```bash
cargo test -p worker-pool --test skill_adapter
cargo test -p worker-pool --test pool
cargo test -p worker-pool
```

- [ ] **Step 5: Commit the adapter**

```bash
git add Cargo.lock crates/worker-pool
git commit -m "feat: adapt browser workers for Bobby skills"
```

### Task 5: Recover durable skill recovery

**Files:**
- Modify: `crates/checkpoint-store/Cargo.toml`
- Modify: `crates/checkpoint-store/src/lib.rs`
- Modify: `crates/checkpoint-store/tests/checkpoints.rs`
- Modify: `crates/page-runtime/Cargo.toml`
- Modify: `crates/page-runtime/src/lib.rs`
- Modify: `crates/page-runtime/src/recovery.rs`
- Create: `crates/page-runtime/src/skill_recovery.rs`
- Create: `crates/page-runtime/src/skill_recovery/checkpoint.rs`
- Create: `crates/page-runtime/src/skill_recovery/outcome.rs`
- Create: `crates/page-runtime/src/skill_recovery/tactics.rs`
- Create: `crates/page-runtime/tests/skill_recovery.rs`
- Modify: `crates/page-runtime/tests/checkpoints.rs`

**Interfaces:**
- Consumes: current verified checkpoints, worker pool, workflow journal, Task 3 state and tactic controller.
- Produces: `SkillRecoveryCoordinator`, `SkillRecoveryExecution`, `SkillTacticEffect`, durable recovery receipts, reconciliation, and exactly-once finalization.

- [ ] **Step 1: Add recovery tests and checkpoint assertions first**

Port the dedicated legacy recovery test and skill-specific assertions. Retain current checkpoint schema compatibility tests.

- [ ] **Step 2: Verify RED**

```bash
cargo test -p page-runtime --test skill_recovery
```

Expected: missing `SkillRecoveryCoordinator` and receipt persistence behavior.

- [ ] **Step 3: Port checkpoint persistence extensions**

Add durable issued-decision and recovery-receipt storage without removing current atomic checkpoint replacement, schema migration, or lineage validation.

- [ ] **Step 4: Port recovery coordinator modules**

Adapt legacy recovery tactics to current `PageRuntime`, command envelope, checkpoint gate, worker replacement, journal, and observability interfaces. Preserve serialized execution gates, bounded cleanup, effect reconciliation, outbox recovery, and receipt replay.

- [ ] **Step 5: Run focused recovery tests**

```bash
cargo test -p checkpoint-store
cargo test -p page-runtime --test skill_recovery
cargo test -p page-runtime --test checkpoints
cargo test -p page-runtime
```

- [ ] **Step 6: Commit durable recovery**

```bash
git add Cargo.lock crates/checkpoint-store crates/page-runtime
git commit -m "feat: restore durable Bobby skill recovery"
```

### Task 6: Recover interface and SDK integration

**Files:**
- Modify: `crates/sdk-core/src/interface.rs`
- Modify: `crates/sdk-core/src/lib.rs`
- Modify: `crates/sdk-core/tests/interface_api.rs`
- Modify: `crates/sdk-core/tests/recovery_api.rs`
- Modify: `crates/mcp-gateway/src/schema.rs`
- Modify: `crates/mcp-gateway/tests/tools.rs`
- Modify: `crates/interface-conformance/tests/mcp_live.rs`
- Modify: `crates/interface-conformance/tests/rust_sdk.rs`
- Modify: `crates/broker/tests/commands.rs`

**Interfaces:**
- Consumes: current authenticated command surfaces and Task 2 skill envelopes.
- Produces: capability-checked skill command dispatch and SDK recovery access without adding new authority.

- [ ] **Step 1: Port interface tests first**

Add only skill command, recovery receipt, schema bound, and authority assertions from the legacy branch.

- [ ] **Step 2: Verify RED**

```bash
cargo test -p sdk-core --test recovery_api
cargo test -p mcp-gateway --test tools
```

Expected: missing public skill/recovery APIs or schema variants.

- [ ] **Step 3: Port minimal interface code**

Map skill commands through existing authenticated dispatch and capability checks. Preserve current MCP HTTP/stdio behavior, exact schema bounds, and multi-principal isolation.

- [ ] **Step 4: Run interface gates**

```bash
cargo test -p sdk-core
cargo test -p mcp-gateway
cargo test -p interface-conformance
cargo test -p broker
```

- [ ] **Step 5: Commit interface integration**

```bash
git add crates/sdk-core crates/mcp-gateway crates/interface-conformance crates/broker
git commit -m "feat: expose Bobby skills through runtime interfaces"
```

### Task 7: Recover the deterministic gauntlet package

**Files:**
- Create: `packages/bobby-gauntlet/**`
- Modify: `pnpm-lock.yaml`

**Interfaces:**
- Produces: `GauntletStation`, `GauntletManifest`, `GauntletController`, `GauntletScorecard`, `createFoundationController`, and ten independently runnable station routes.

- [ ] **Step 1: Add the package manifest, fixtures, and tests only**

Restore `package.json`, `tsconfig.json`, approved fixture, and all files under `packages/bobby-gauntlet/test/`. Do not add `src/` yet.

- [ ] **Step 2: Install lockfile metadata and verify RED**

```bash
pnpm install
pnpm --dir packages/bobby-gauntlet test
```

Expected: TypeScript module resolution fails because gauntlet implementation modules are absent.

- [ ] **Step 3: Port core contracts, manifest, controller, and scorecard**

Restore deterministic seeded setup, immutable manifests, bounded evidence IDs, typed failures, non-concealing aggregate scoring, and station registration checks.

- [ ] **Step 4: Port all ten stations and browser application**

Restore the station modules, application mount, HTML routes, redirect route, file fixture handling, generated download, and championship composition.

- [ ] **Step 5: Run package gates**

```bash
pnpm --dir packages/bobby-gauntlet typecheck
pnpm --dir packages/bobby-gauntlet test
pnpm --dir packages/bobby-gauntlet build
```

- [ ] **Step 6: Commit the gauntlet package**

```bash
git add pnpm-lock.yaml packages/bobby-gauntlet
git commit -m "feat: restore deterministic Bobby browser gauntlet"
```

### Task 8: Recover production championship bindings

**Files:**
- Modify: `crates/runtime-tests/Cargo.toml`
- Create: `crates/runtime-tests/tests/bobby_skill_recovery.rs`
- Create: `crates/runtime-tests/tests/bobby_skills_gauntlet.rs`
- Modify: selected existing runtime tests only where current APIs require shared skill fixtures.

**Interfaces:**
- Consumes: Tasks 3–7 production runtime and static gauntlet.
- Produces: production recovery and seeded championship certification.

- [ ] **Step 1: Port production runtime tests first**

Add the two dedicated legacy test files and adapt imports to current public APIs without weakening assertions.

- [ ] **Step 2: Verify RED**

```bash
cargo test -p runtime-tests --test bobby_skill_recovery
cargo test -p runtime-tests --test bobby_skills_gauntlet
```

Expected: failures identify missing production bindings or changed interface assumptions.

- [ ] **Step 3: Add only the required production bindings**

Wire production `PageRuntime`, checkpoint storage, worker replacement, skill state, and gauntlet station outcomes. Do not substitute a test-only coordinator.

- [ ] **Step 4: Run certification tests**

```bash
cargo test -p runtime-tests --test bobby_skill_recovery
cargo test -p runtime-tests --test bobby_skills_gauntlet
cargo test -p runtime-tests
```

- [ ] **Step 5: Commit championship integration**

```bash
git add Cargo.lock crates/runtime-tests
git commit -m "test: certify Bobby skill recovery against the gauntlet"
```

### Task 9: Publish Skills and Gauntlet documentation

**Files:**
- Create: `docs/bobby-browser/source/pages/guides/skills.md`
- Create: `docs/bobby-browser/source/pages/guides/gauntlet.md`
- Modify: `docs/bobby-browser/source/navigation.json`
- Modify: `docs/bobby-browser/source/pages/guides/events-recovery.md`
- Modify: `docs/bobby-browser/source/pages/guides/run.md`
- Modify: `README.md`
- Generated: `docs/bobby-browser/v0.2.0/**`

**Interfaces:**
- Consumes: implemented CLI/SDK commands, failure codes, package scripts, and scorecard schema.
- Produces: source-of-truth public guidance and synchronized immutable docs artifact.

- [ ] **Step 1: Extend docs tests to require the two new pages and links**

Modify `scripts/docs/bobby-browser-docs.test.mjs` so source navigation, versioned navigation, README links, and generated manifests must include `guides/skills.md` and `guides/gauntlet.md`.

- [ ] **Step 2: Verify RED**

```bash
pnpm docs:test
```

Expected: missing source pages/navigation entries.

- [ ] **Step 3: Author source documentation**

Document exact slash commands, activation scope, effective profile status, recovery ladder, typed failures, capability boundaries, local gauntlet test/build commands, seeded manifests, scorecards, ten stations, and live-browser prerequisites.

- [ ] **Step 4: Update source navigation and repository entry points**

Add concise links from README and relevant run/recovery pages. Keep implementation details and recovery history out of public product docs.

- [ ] **Step 5: Generate and verify versioned docs**

```bash
pnpm docs:build
pnpm docs:verify
pnpm docs:test
```

- [ ] **Step 6: Commit public docs**

```bash
git add README.md docs/bobby-browser scripts/docs
git commit -m "docs: publish Bobby skills and gauntlet guides"
```

### Task 10: Full verification and recovery audit

**Files:**
- Verify all files changed from `origin/main`.
- Update: this plan's checkboxes as tasks complete.

**Interfaces:**
- Consumes: all recovered subsystems.
- Produces: evidence that no legacy feature or current public feature was silently lost.

- [ ] **Step 1: Audit recovered legacy surface**

```bash
git diff --stat origin/main...HEAD
git diff --name-status origin/main...HEAD
git log --oneline origin/main..HEAD
git diff --check
```

Compare the result with `git diff --stat main...feat/bobby-skills-gauntlet` and account for every skill-specific production, test, package, and architecture-doc file.

- [ ] **Step 2: Run all Rust gates**

```bash
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

- [ ] **Step 3: Run all JavaScript and documentation gates**

```bash
pnpm --dir packages/bobby-gauntlet typecheck
pnpm --dir packages/bobby-gauntlet test
pnpm --dir packages/bobby-gauntlet build
pnpm docs:build
pnpm docs:verify
pnpm docs:test
```

- [ ] **Step 4: Run supported live-browser gates**

```bash
cargo test -p runtime-tests --test bobby_skills_gauntlet -- --ignored --nocapture
```

If browser prerequisites are unavailable, record the exact skipped command and environment error without claiming it passed.

- [ ] **Step 5: Review final status**

```bash
git status --short --branch
git diff --check origin/main...HEAD
```

The branch is ready for review only when tracked changes are intentional, ordinary gates pass, generated docs match source, and environment-dependent exceptions are explicitly reported.
