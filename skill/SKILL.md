---
name: bobby-browser
description: >
  Drive the bobby-browser automation runtime over its MCP surface. Use whenever
  the task involves browsing a page, filling or submitting a form, extracting
  page data, taking screenshots, reading cookies or network logs, hitting a
  captcha or verification widget, or checkpointing and recovering a browser
  workflow. Covers session setup, intent tools, evidence, error handling, and
  recovery.
---

bobby-browser is a browser automation runtime, not an agent. You drive it
through MCP tools; the runtime verifies every effect and returns evidence.
Never claim an action worked without its evidence.

## Start here

On MCP `initialize`, the server's `instructions` names the canonical first
calls. Rules that govern every call:

1. **Always read `structuredContent` before the next mutating call.** A
   `status` that is not `completed` is a failure even when prose looks
   optimistic (`isError: true` marks it too). Repair from `error.repair`
   (`{action, doc}`); the doc points into `bobby://failure-taxonomy`.
2. **Pull references on demand, not upfront.** All match the build:
   `bobby://intents` (the ten intent tools and what each verifies),
   `bobby://primitives`, `bobby://failure-taxonomy`, `bobby://capabilities`,
   `bobby://job-handlers`.
3. **Batch deferred-tool discovery in one search.** If your host defers MCP
   tool schemas behind a tool search, issue ONE search selecting every tool
   the task will need (`select:` accepts a comma-separated list) — each extra
   round trip is a full model turn. The explore toolset already advertises
   the standard loop (observe, navigate, click, type, upload, dialogs,
   downloads, `intent_complete_form`, `intent_submit_and_verify`,
   `intent_detect_challenge`); search only for what is genuinely missing.

## Core loop

- `workflow_start` (optionally with `url`) creates session + page + workflow
  and returns a `workflowHandle`. Use it over manual `session_create` +
  `page_open`. Prefer `workflow_observe` for context: it answers from
  retained page memory first and only pays for a live snapshot when nothing
  is remembered.
- **Read before write.** On a site this runtime has seen before, `context_ask`
  first — a remembered answer (marked `persisted`) beats a snapshot.
  Otherwise `a11y_snapshot`, and pass its targets straight into intent or
  primitive calls — never guess selectors.
- A snapshot node's `target` `{role, accessibleName, ordinal}` goes verbatim
  into any `intent_*` `hints` or a primitive's `target`. Keep `ordinal`; it
  is what separates duplicate role/name pairs.
- **Scope big pages**: pass `target: {role: "main"}` to `workflow_observe`
  to skip repeated site chrome, and `target` on `a11y_snapshot` to scope to
  one form or dialog.
- **Trim observation payloads when it counts.** `workflow_observe` and
  `intent_complete_form` default `evidenceDetail: "compact"` on success;
  pass `"full"` only when debugging.
- Pass the returned `workflowHandle` on later calls. `sessionId`/`pageId`/`workflowId` are the repair path if the handle stops resolving (handles expire with the server generation; explicit ids survive).

## Choosing the tool

- Form with multiple fields: one `intent_complete_form` (fields resolve
  just-in-time; include conditional fields after their revealer even if
  initially absent) — never a `intent_fill` per field unless fields must
  resolve in reaction to each other.
- Submit: `intent_submit_and_verify` with an `expectedState` that only holds
  after the submit (a confirmation id, status change, or new element).
- Data out: `intent_extract` (named fields, per-field errors) or
  `extract_structured` (schema-shaped JSON via vision — needs `vision:assist`).
- A popup/overlay blocks the page: `intent_dismiss_obstruction`.
- **A captcha or verification widget blocks the page:**
  `intent_detect_challenge` (also advertised in explore) classifies it
  read-only; `intent_solve_challenge` runs the vision solve loop. Both need
  `vision:assist` plus the session's `executionPolicy.visionAssist` — the
  capability alone is not enough. The runtime never bypasses a challenge;
  when the solve loop cannot clear it, surface the page to the operator.

## Rules that bite

1. **Boundary submits are checkpoint-gated.** `intent_submit_and_verify` and
   `intent_follow` with `boundary: true` refuse to run without a matching
   checkpoint. `autoCheckpoint` defaults to `true` and mints it inside the
   same call. Hand-author one (`autoCheckpoint: false`) only to attach
   `invariants`/`replayableInputs`; put commands you already ran in
   `evidenceRefs` — never hand-authored evidence.
2. **Reuse the `workflowId`.** Every outcome echoes it; pass it back so
   `checkpoint_save`/`workflow_recover` see the whole flow. Lost it?
   `recovery_status` with `sessionId` lists that session's recoverable
   workflows, newest first.
3. **Fail-closed by design.** `verificationFailed` means the action ran but
   the expected state was not proven — re-read (`inspect`, `form_snapshot`)
   instead of retrying blindly. `needsReconciliation` means the side effect
   may have landed — call `recovery_status`, never replay the command.
4. **A browser hiccup is not your problem to fix.** `targetDetached` /
   "browser page is not open" after a transport reset means the runtime
   already reattached (page state, including typed values, is preserved —
   `cdpReattach` evidence) or relaunched (state wiped, page reloaded to its
   last URL). Re-observe, then continue; do not restart the whole flow, and
   do not re-run anything a `needsReconciliation` already reported as
   possibly-landed.
5. **Artifacts are evidence.** Screenshots, PDFs, HAR captures, and downloads
   come back as digest-verified artifacts (`artifact://<id>`). When a
   download must land as a file, pass `saveAs` to `download_url` — it
   rejects escapes or overwrites before fetching, and `savedTo` + `sha256`
   mean no shell verification is needed.

## Error signals

| Signal | Meaning | Repair |
|---|---|---|
| `missingCapability` | Token lacks a required capability | Re-issue credential with that capability, or pick a covered tool |
| `authenticationFailed` / `tokenExpired` | Credential bad or expired | Operator re-runs `bobby init --force`; reconnect |
| `targetNotFound` / `targetAmbiguous` | Stale or guessed target | Fresh `a11y_snapshot`; pass the new target verbatim |
| `verificationFailed` | Action ran; expected state not proven | Re-inspect; adjust expectation or fill; do not blind-retry |
| `boundaryAlreadyExecuted` | A prior submit for this workflow + control completed | Inspect the named prior outcome; pass `reSubmit: true` only for a genuinely intended second effect on the same control |
| `needsReconciliation` | Side effect may already have landed | Call `recovery_status`; never retry the Boundary command |
| `targetDetached` / page-level `notFound` | Transport reset or stale page | Reattach/relaunch already handled; re-observe, continue with current ids |
| `deadlineExceeded` | Deadline elapsed | Longer deadline; retry only if Replayable |
| `idempotencyConflict` | Same key, different body | Mint a fresh idempotency key |

When unsure, open `bobby://failure-taxonomy` — tool descriptions give the
precise repair for their failure modes and win over this table.

## Anti-patterns

- Claiming success from a chat summary without `status: completed` evidence.
- Blind-retrying after `verificationFailed` or any `needsReconciliation`.
- Inventing CSS/XPath selectors instead of snapshot targets.
- One `intent_fill` per field where `intent_complete_form` would do.
- Re-logging into sites every session instead of using the operator's paired
  Firefox profile (disposable Chromium profiles keep no cookies — see below).
- Hand-authoring `evidenceRefs` or forging artifact digests.
- Retrying the same tool with the same token after `missingCapability`.

## Engine and persistence (skim once)

The gateway resolves the browser engine at startup: an explicit
`AUTOMATION_RUNTIME_BROWSER_SELECTION`, else the operator's paired enrollment
(`browser-selection.json`), else fail-closed. Two engines result:

- **Firefox companion:** real headed Firefox, persistent profile — cookies
  and logins survive sessions. Sign in once in that window.
- **Managed Chromium:** disposable per-session profile. Nothing persists. If
  logins vanish between sessions, ask the operator to Pair the Firefox
  companion instead of re-authenticating every run.

Setup, pairing, and credential minting are operator tasks (`bobby install`,
`bobby doctor`); if the runtime is unreachable, say `bobby doctor` names the
broken piece. Scheduler jobs (`job_submit`/`job_status`/`job_cancel`) are
scheduler probes, not browser intents — see `bobby://job-handlers`.