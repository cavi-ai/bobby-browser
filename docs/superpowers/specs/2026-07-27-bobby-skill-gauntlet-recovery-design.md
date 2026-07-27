# Bobby Skill Gauntlet Recovery Design

## Objective

Recover the complete legacy Bobby skill subsystem and browser gauntlet onto the
canonical public-history branch without regressing functionality added after the
legacy branch diverged. The recovered system must retain its production recovery
binding, security properties, deterministic tests, and public documentation.

## Source and Target

- Target baseline: `origin/main`, whose rewritten two-commit history is canonical.
- Recovery source: `feat/bobby-skills-gauntlet`, containing 33 branch-only commits.
- Target branch: `feat/bobby-skill-gauntlet`.
- The legacy branch and its worktree remain unchanged as recovery evidence.

Commit ancestry is not evidence of missing public functionality because the
public repository deliberately replaced its history. Every conflict is resolved
by comparing behavior and current interfaces, not by preferring the legacy tree.

## Recovery Strategy

Replay the 33 legacy commits in order where practical. Resolve each conflict
against the current public implementation and preserve commit boundaries when
they still represent independently testable behavior. If a legacy commit depends
on an interface that has since changed, port its behavior to the current
interface instead of restoring the obsolete API.

New public functionality wins in overlapping files. In particular, the recovery
must preserve the current intent engine, richer network waits, closed-shadow-root
support, observability, lifecycle hardening, multi-principal security, release
metadata, and public documentation conventions.

## Runtime Architecture

The recovery adds the versioned skill subsystem as an engine-neutral layer:

1. Stable skill contracts describe capabilities, configuration, decisions,
   outcomes, evidence, failure classes, and durable session state.
2. `skill-runtime` owns slash-command parsing, registry validation, SkillGhost,
   SkillZigZagZig, and state transitions.
3. Worker-pool adapters translate requested skill capabilities into effective
   browser configuration without expanding workflow authority.
4. Page-runtime recovery owns checkpoints, budgets, tactic ordering, attempt
   lineage, reconciliation, and terminal classification.
5. The SDK and interface layers expose the same bounded contracts without making
   skills an independent policy authority.

`SkillGhost` freezes a coherent browser profile for a session and reports the
effective configuration. `SkillZigZagZig` applies a deterministic, bounded
recovery ladder and never blindly replays an uncertain mutation.

## Bobby Gauntlet

Restore `packages/bobby-gauntlet` as a deterministic static application with a
seeded immutable manifest, isolated station routes, verified results, and a
replayable scorecard. The initial course includes route navigation, DOM drift,
semantic form completion, validation repair, iframe traversal, open shadow DOM,
popup handling, file attachment, generated download, and championship workflow.

Each station remains independently runnable. Aggregate success requires every
mandatory station to pass, so a score cannot conceal a failed requirement. The
production runtime championship tests must exercise real recovery integration,
not a test-only substitute.

## Security and Failure Handling

- Page content cannot activate skills, mint capability grants, alter budgets, or
  declare its own result successful.
- Skill activation never expands principal or workflow authority.
- Configuration, manifests, evidence, and scorecards are bounded, versioned, and
  validated.
- Secrets, cookies, bearer tokens, unrelated page content, and unrestricted host
  paths do not enter skill evidence or scorecards.
- File uploads use approved handles; downloads remain scoped to run artifacts.
- Required unsupported capabilities fail closed before execution.
- Uncertain mutations are reconciled from a verified checkpoint or stopped with
  a typed failure; they are never retried blindly.
- Browser replacement and cleanup remain bounded by the workflow deadline.

## Public Documentation

Documentation follows the canonical generated-docs system:

1. Add and edit curated pages only under `docs/bobby-browser/source/`.
2. Add Skills and Gauntlet pages to the source navigation manifest.
3. Update the repository README and relevant recovery/run guidance with concise
   links to the curated pages.
4. Rebuild the immutable versioned artifact with `pnpm docs:build`.
5. Validate it with `pnpm docs:verify` and `pnpm docs:test`.
6. Never hand-edit files under `docs/bobby-browser/v*/`.

The public docs explain skill activation, status, failure semantics, security
boundaries, local gauntlet operation, deterministic manifests, scorecards, and
the distinction between diagnostic tests and release qualification. Internal
recovery notes and implementation plans remain under `docs/superpowers/` and do
not become product documentation.

## Verification

Recovery proceeds through focused red-green cycles. Existing legacy tests are
ported before their corresponding production behavior and must fail for the
expected missing behavior on the public baseline.

Verification layers are:

1. Skill contract and serialization tests.
2. Registry, router, SkillGhost, SkillZigZagZig, and state tests.
3. Worker adapter and page-runtime recovery tests.
4. Seeded gauntlet unit, browser-model, security, and championship tests.
5. Production runtime skill-recovery and championship tests.
6. `cargo test --workspace`.
7. Gauntlet typecheck, test, and build commands.
8. Documentation build, verification, and test commands.

Ignored live-browser tests are reported separately because they require installed
browser infrastructure. They are run when the local environment supports their
declared prerequisites.

## Acceptance Criteria

- The full 33-commit behavior is represented on the public baseline without
  removing newer public functionality.
- `/ghost on|off|status` and `/zigzagzig run|status|stop` use a modular registry.
- SkillGhost reports an effective coherent profile and fails closed for unmet
  required capabilities.
- SkillZigZagZig respects deadlines and budgets, records its decisions, and does
  not blindly replay uncertain mutations.
- Firefox and Chromium use the same engine-neutral adapter contract.
- All ten deterministic stations and the championship route are available.
- Every mandatory station produces a verified result or typed failure.
- Production recovery completes the seeded championship and emits a replayable
  scorecard.
- Public source docs, navigation, generated versioned docs, and README links are
  synchronized and pass the repository documentation gates.
- Focused tests, the workspace test suite, package tests/build, and docs gates
  pass, apart from explicitly reported environment-dependent live-browser gates.
