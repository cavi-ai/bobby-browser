---
name: bobby-browser
description: >
  Drive the bobby-browser automation runtime over its MCP surface. Use whenever
  the task involves browsing a page, filling or submitting a form, extracting
  page data, taking screenshots, reading cookies or network logs, or
  checkpointing and recovering a browser workflow. Covers session setup,
  the intent tools, evidence, and recovery.
---

# bobby-browser

bobby-browser is a browser automation runtime, not an agent. You drive it
through MCP tools; the runtime verifies every effect and returns evidence.
Never claim an action worked without its evidence.

## Setup

1. `bobby init --emit claude` (or `zed` / `vscode` / `json`) writes the
   bootstrap credential and prints the MCP client config fragment. Merge the
   fragment into the host's MCP config and source `bootstrap.env` into the
   host's environment so the `${...}` placeholders resolve.
2. `bobby doctor` validates the whole setup, including an MCP handshake
   (`initialize` + `tools/list`) against the gateway.

## How the runtime is wired

Your host spawns `bobby mcp-stdio`, which loads the bootstrap credential and
execs the stdio gateway in-place. The gateway runs the entire runtime
in-process: it loads `config.toml` (`BOBBY_BROWSER_CONFIG`, else
`./config.toml`) and picks a browser engine exactly the way `bobby serve` and
`bobby doctor` do — one shared resolution order:

1. `AUTOMATION_RUNTIME_BROWSER_SELECTION` (JSON) — an override; wins when set.
2. The persisted enrollment at
   `<config-dir>/bobby-browser/browser-selection.json`, written by
   `bobby enroll-firefox-profile`.
3. The built-in default: exact Firefox, fail-closed. With nothing enrolled,
   startup fails with an actionable error rather than silently downgrading.

A malformed source is always an error, never skipped. `bobby doctor` reports
which source resolved, so what it validates is what you are running.

### Which engine you are driving

- **Firefox companion (the default, and what enrollment sets up):** the
  gateway attaches to a real, visible Firefox over WebDriver BiDi — a real
  profile directory with real cookies. Logins persist across sessions. Sign
  in once in that window; every later session is already authenticated.
- **Managed Chromium (fallback when explicitly selected):** each session gets
  a disposable profile keyed by session id. Nothing persists — no cookies, no
  logins — by design.

If a site demands a login and cookies vanish between sessions, you are on the
Chromium path: ask the operator to run `bobby enroll-firefox-profile` (Firefox
with `--remote-debugging-port`, the companion extension installed). Do not
work around it by re-logging-in every session.

## Working loops

Read these resources first; they are authoritative and always match the build:

- `bobby://capabilities` — what each capability gates. A `missingCapability`
  error means the principal's token lacks the named capability.
- `bobby://intents` — the eight intent tools and what each verifies.
- `bobby://failure-taxonomy` — every error code and its repair action.
- `bobby://primitives` — the flat browser tools.

Three prompts encode the standard flows: `fill_and_submit_form`,
`extract_from_page`, `recover_workflow`.

## Rules that bite

1. **Checkpoint before boundaries.** `intent_submit_and_verify` and
   `intent_follow` with `boundary: true` are Boundary commands: call
   `checkpoint_save` (with `evidenceRefs` from the commands you ran, not
   hand-authored evidence) before them, or recovery cannot resume the flow.
2. **Reuse the `workflowId`.** Every envelope-minting tool returns one; pass
   it back so `checkpoint_save` / `workflow_recover` see the whole flow.
3. **Fail-closed by design.** `verificationFailed` means the page did not end
   in the state you asked for — re-read the page (`inspect`, `a11y_snapshot`)
   instead of retrying blindly. `needsReconciliation` means stop and ask a
   human; do not replay the command.
4. **Read before write.** Take an `a11y_snapshot`, pass its targets straight
   into `click` / `type_text` / `upload_files` — no selector guessing.
5. **Artifacts are evidence.** Screenshots, PDFs, HAR captures, and downloads
   come back as digest-verified artifacts, readable as `artifact://<id>`
   resources with `artifact:read`.
