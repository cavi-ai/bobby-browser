---
name: bobby-browser
description: >
  Drive the bobby-browser automation runtime over its MCP surface. Use whenever
  the task involves browsing a page, filling or submitting a form, extracting
  page data, taking screenshots, reading cookies or network logs, or
  checkpointing and recovering a browser workflow. Covers session setup,
  intent tools, evidence, error handling, and recovery.
---

# bobby-browser

This is the **public agent skill** (`bobby install --skill`). It is not
Ghost / ZigZagZig recovery (Rust: `SkillGhost` / `SkillZigZagZig`) — those are
in-process recovery strategies and are not MCP tools.

Jobs: `job_submit` / `job_status` / `job_cancel` (needs `job:*` caps). They
advertise after `toolset_select` → `verify`. Same contract as HTTP `/v1/jobs`.
Built-in handlers today: `echo` (returns payload) and `sleep` (`payload.ms`).
`bobby doctor` lists them under `job-handlers`.

Release layout is three binaries (`bobby`, `mcp-gateway`, `acp-gateway`). Keep
`tools/list` small with Explore (`toolset_select` / `BOBBY_MCP_TOOLSET=explore`)
unless you need the full catalog.

bobby-browser is a browser automation runtime, not an agent. You drive it
through MCP tools; the runtime verifies every effect and returns evidence.
Never claim an action worked without its evidence.

## Setup

1. Operator runs `bobby install` (or `bobby install --host <claude|zed|vscode|acp>
   --skill --yes`). That writes the bootstrap credential, merges MCP config,
   and installs this skill into `~/.agents/skills/bobby-browser/` (project:
   `.agents/skills/` with `--project-skill`). Optional: `--skill-claude`,
   `--skill-openclaw`. For agent hosts that should not mint tokens, prefer
   `bobby init --preset agent` (no `authority:admin`; heal respects the
   marker). Default `bobby init` remains unrestricted.
2. For the Firefox companion (persistent logins): `bobby install --companion`
   or `make firefox`, start Firefox with `--remote-debugging-port` (`make
   firefox-start` if using the launchd agent), then **Pair** from the toolbar
   popup. That writes `browser-selection.json`. CLI
   `bobby enroll-firefox-profile` is for CI/scripting only.
3. `bobby doctor` validates the setup, including sibling gateway presence
   (`mcp-gateway` / `acp-gateway`), bootstrap preset, and an MCP handshake
   (`initialize` + `tools/list`) against the gateway. When `vision:assist` is
   held, doctor also reminds that sessions need
   `executionPolicy.visionAssist=true` — the cap alone does not enable vision.
   Same for `javascript:evaluate` and `executionPolicy.javascriptEvaluation`.

Your host should spawn `bobby mcp-stdio` (loads the credential itself). Prefer
that over raw `mcp-gateway` + env placeholders. ACP hosts use `bobby acp-stdio`
the same way.

## How the runtime is wired

`bobby mcp-stdio` execs the stdio gateway in-process. The gateway loads
`config.toml` (`BOBBY_BROWSER_CONFIG`, else `./config.toml`) and resolves the
browser engine the same way `bobby serve` and `bobby doctor` do:

1. `AUTOMATION_RUNTIME_BROWSER_SELECTION` (JSON) — override; wins when set.
2. Persisted enrollment at
   `<config-dir>/bobby-browser/browser-selection.json` (written by popup
   **Pair** or `bobby enroll-firefox-profile`).
3. Built-in default: exact Firefox, fail-closed. With nothing enrolled,
   startup fails with an actionable error rather than silently downgrading.

A malformed source is always an error, never skipped.

### Which engine you are driving

- **Firefox companion (default after Pair):** real headed Firefox over
  WebDriver BiDi with a real profile — cookies and logins persist. Sign in
  once in that window; later sessions stay authenticated.
- **Managed Chromium (only when explicitly selected):** disposable profile
  per session. Nothing persists.

If a site needs login and cookies vanish between sessions, you are on
Chromium: ask the operator to Pair the Firefox companion. Do not work around
it by re-logging-in every session.

## Working loops

Read these resources first; they always match the build:

- `bobby://capabilities` — what each capability gates.
- `bobby://intents` — the eight intent tools and what each verifies.
- `bobby://failure-taxonomy` — every error code and its repair action.
- `bobby://primitives` — the flat browser tools.

Three prompts encode the standard flows: `fill_and_submit_form`,
`extract_from_page`, `recover_workflow`.

## Rules that bite

1. **Checkpoint before boundaries.** `intent_submit_and_verify` and
   `intent_follow` with `boundary: true` are Boundary commands: refused
   without a matching checkpoint. `autoCheckpoint` defaults to `true` and
   mints it inside the same call, returning its `checkpointId`. Pass
   `autoCheckpoint: false` only when you need to author the checkpoint's
   `invariants` or `replayableInputs`: pin two UUIDs, pass them as
   `commandId`/`attemptId` to both `checkpoint_save`
   (`boundaryCommandId`/`attemptId` in the checkpoint) and the Boundary
   call, and put commands you already ran in `evidenceRefs` — never
   hand-authored evidence.
2. **Reuse the `workflowId`.** Every outcome echoes `workflowId`,
   `commandId`, and `attemptId`; pass `workflowId` back so
   `checkpoint_save` / `workflow_recover` see the whole flow. Lost it to a
   restart or a compaction? `recovery_status` with `sessionId` instead of
   `workflowId` lists that session's recoverable workflows, newest first.
3. **Failed commands set `isError: true`.** A tool result whose
   `structuredContent.status` is not `completed` is a failure — read the
   `error.code` and repair via `bobby://failure-taxonomy`; do not continue
   as if it succeeded.
4. **Fail-closed by design.** `verificationFailed` means the page did not end
   in the state you asked for — re-read (`inspect`, `a11y_snapshot`) instead
   of retrying blindly. `needsReconciliation` means stop and ask a human; do
   not replay the command.
5. **Read before write.** On a site this runtime has seen before, ask
   `context_ask` first — a remembered answer (marked `persisted`) beats a
   snapshot. Otherwise take an `a11y_snapshot`, pass its targets straight
   into `click` / `type_text` / `upload_files` — no selector guessing.
6. **A snapshot target is a valid hint.** A node's `target`
   (`{role, accessibleName, ordinal}`) goes verbatim into an `intent_*`
   tool's `hints` — every field lands. Keep `ordinal`; it is what separates
   duplicate role/name pairs.
7. **Artifacts are evidence.** Screenshots, PDFs, HAR captures, and downloads
   come back as digest-verified artifacts (`artifact://<id>` via
   `artifact:read`). The `bobby://` docs are readable by any principal.

## Error handling

Always inspect `structuredContent` (status, error code, evidence) before the
next mutating call.

| Signal | Meaning | Repair |
|---|---|---|
| `missingCapability` | Token lacks a required capability | Re-issue credential with that capability, or pick a covered tool |
| `authenticationFailed` / `tokenExpired` | Bootstrap credential bad or expired | Operator re-runs `bobby init --force`; reconnect |
| `targetNotFound` / ambiguous target | Stale or guessed selector | Fresh `a11y_snapshot`; pass the new target |
| `verificationFailed` | Action ran; expected state not proven | Re-inspect; adjust expectation or fill; do not blind-retry |
| `needsReconciliation` | Side effect may already have landed | Call `recovery_status`; never retry the Boundary command |
| `deadlineExceeded` | Deadline before/during dispatch | Longer deadline; retry only if Replayable |
| `idempotencyConflict` | Same key, different body | Mint a fresh idempotency key |

When unsure, open `bobby://failure-taxonomy` — tool-specific descriptions win
over the general table.

## Anti-patterns

- Claiming success from a chat summary without evidence / `status: completed`.
- Blind-retrying after `verificationFailed` or any `needsReconciliation`.
- Calling Boundary tools without a prior matching `checkpoint_save`.
- Inventing CSS/XPath selectors instead of snapshot targets.
- Re-logging into sites every session instead of using the paired Firefox
  profile.
- Treating Chromium disposable profiles as if they keep cookies.
- Hand-authoring `evidenceRefs` or forging artifact digests.
- Ignoring `isError: true` because the prose message looked optimistic.
- Skipping `bobby://capabilities` after a `missingCapability` and retrying
  the same tool with the same token.
