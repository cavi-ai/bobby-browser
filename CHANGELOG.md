# Changelog

## Unreleased
- **Breaking (Rust):** the `/v1` wire types moved from the `types` crate into `bobby-browser-client`, which is now the single published Rust crate (`cargo publish` dry-run verified; `types` remains in the workspace as a `publish = false` re-export shim over the moved modules). crates.io publishing is now the one `bobby-browser-client` crate instead of the 25-crate ordered closure.
- TypeScript SDK source now carries JSDoc on the public surface (client, contracts, errors, events, intents, validators).
- **Breaking:** `executionPolicy.fingerprint` and `executionPolicy.humanize` now require the new `browser:fingerprint` and `browser:humanize` capabilities at session creation; a principal without them gets `missingCapability` and no session is created. `bobby init` bootstrap credentials include both, matching the `vision:assist` double-gate precedent.
- Document the MCP surface depth shipped in v0.3.1: per-tool `outputSchema`, `title` + `annotations`, the four `bobby://` resources, `artifact://` capture resources, the three working-loop prompts, and `notifications/bobby/event` + `notifications/tools/list_changed` push channels. Document `job:*` capabilities and the `browser:fingerprint` / `browser:humanize` gates in the capabilities concept page.
- Add a per-session context graph: `a11y_snapshot` results are retained per page and answer "where is the control described as X" with a bound target plus a confidence score.
- The graph invalidates on any command not on an explicit read-only allowlist, including `navigate` and `emulate`, which are `CommandClass::Replayable` yet change the page.
- A failed non-read-only command invalidates too, since a command that failed is not a command that did nothing.
- Truncated accessibility snapshots are not recorded, so the graph never reports a control absent when it was cut off.
- Ambiguous, partial, and below-floor matches answer nothing rather than guessing.
- Retained page context is dropped when its session is deleted, and bounded at 256 pages so an unclosed page cannot leak page text for the life of the process.
- Add a `[nodes.<name>]` config table: named, separately addressable nodes with `kind` (`vision`), `endpoint_url`, optional `token_env`, and `timeout_ms`. An unknown kind fails config load.
- Add `executionPolicy.visionNode`, naming which registered node a session escalates to.
- A named node that is unknown, or configured with the wrong kind, declines the escalation and never falls back to another node or to a process-wide provider.
- A `[vision]` endpoint with no `[nodes]` table is reachable as a node named `vision`; when both are set `[nodes]` wins and `[vision]` is ignored.
- Node locality is derived from the node's address, so a session bound to a loopback node keeps page material on the machine.
- **Breaking (HTTP):** `POST /v1/checkpoints` takes `evidenceRefs` (command ids, max 128) instead of `evidence`. The runtime resolves each id against its own journal and checks session ownership, so a caller can no longer author evidence for work it did not perform. Matches the MCP `checkpoint_save` contract. TypeScript SDK `CheckpointRequest.evidence` is replaced by `CheckpointRequest.evidenceRefs`.
- Add `crates/interface-conformance/tests/checkpoint_evidence.rs`: asserts no adapter accepts caller-authored checkpoint evidence, and that `evidenceRefs` is accepted on each.
- Add `executionPolicy.fingerprint` and `executionPolicy.humanize`, both deny-by-default. Fingerprint spoofing was a process-wide worker-factory setting; it is now per session. Humanized input timing was unconditional on the Firefox path; it is now per session.
- `PageRuntime` writes both flags to the worker on every lease, so a pooled worker never carries one session's opt-in into another's.
- Add `Evidence::Humanization` (`engine`, `actions`, `synthesizedMs`), emitted only when the session opted into `humanize`.
- Add `crates/mcp-gateway/tests/crate_boundary.rs`: fails if a schema names a type from `behavioral-engine`, `fingerprinting`, or `task-scheduler`, none of which carry `JsonSchema` derives.
- Add `networkLog` (MCP `network_log`): always-on bounded per-page network capture (512 entries) on Chromium (CDP Network events) and Firefox (BiDi network events), dumped as a HAR 1.2 artifact.
- Add broker job API `POST|GET|DELETE /v1/jobs` (`job:submit|read|cancel`) with in-process scheduler + optional `scheduler_journal_path`, and CLI `bobby jobs submit|status|cancel`. New bootstrap credentials include `job:*`; `bobby doctor` ensures the scheduler journal dir and warns when bootstrap lacks `job:submit`. Builtin handlers: `echo`, `sleep`.


## 0.3.1 - 2026-08-01
### Documentation
- Publish a new immutable docs artifact that names `@cavi-ai/bobby-browser`
  throughout (the `v0.3.0` release asset still referenced `@bobby-browser/sdk`).
- Carry forward post-`0.3.0` doc coverage already on main (`recovery_status`,
  MCP agent-surface catalog fixes, truncation ordinal notes) into `v0.3.1`.
## Unreleased
- Add cookie primitives (`getCookies`, `setCookies`, `deleteCookies`) on Chromium (CDP Network) and Firefox (BiDi storage), exposed as MCP `cookie_get`/`cookie_set`/`cookie_delete` with `cookieState` evidence.
- Add `printToPdf` (MCP `pdf`) on Chromium (CDP `Page.printToPDF`) and Firefox (BiDi `browsingContext.print`), producing a verified `application/pdf` artifact.
- Add `handleDialog` (MCP `dialog`): waits for a JavaScript dialog with a bounded timeout and accepts or dismisses it, returning dialog type/message/action evidence. Chromium via CDP dialog events, Firefox via BiDi user prompts.
- Add `emulate` (MCP `emulate`): viewport size and geolocation overrides. Chromium via CDP Emulation, Firefox via BiDi viewport and geolocation override.

## 0.3.0 - 2026-08-01

### MCP surface

- Emit only the `$defs` each tool's arguments can reach. A principal holding the default `bobby init` capability set previously produced a `tools/list` past the 1 MiB frame cap, so the gateway answered `resultTooLarge` and no client could enumerate the surface.
- Expose one MCP tool per intent (`intent_locate`, `intent_fill`, `intent_complete_form`, `intent_submit_and_verify`, `intent_wait_for_state`, `intent_follow`, `intent_dismiss_obstruction`, `intent_extract`), each building its own command envelope. `command_execute` still accepts nested intent envelopes.
- Accept an optional `workflowId` on every envelope-minting tool and return it on the outcome, so `checkpoint_save` and `workflow_recover` are reachable without hand-built envelopes.
- Report rejected arguments as `data.pointer` (JSON Pointer) plus `data.constraint`, or as `malformedArguments` / `deadlineOutOfRange` / `invalidIdempotencyKey`, instead of an indistinguishable `"Invalid params"`.
- Add `credentialExpiresAt` to `runtime_info` and a `bootstrap-expiry` check to `bobby doctor` that warns under 7 days and fails once expired.
- Allow MCP `click`, `type_text`, and `upload_files` to consume accessibility-snapshot targets without also requiring a legacy CSS selector.
- Add MCP `recovery_status` (`recovery:read`) alongside `checkpoint_save` / `workflow_recover`.
- Guard MCP schema parity with schemars: `JsonSchema` derives on the wire types and tests that fail when the hand-bounded MCP tool schemas drift from the Rust command/evidence variants.

### Sessions, pages, and events

- Add `DELETE /v1/sessions/{id}`, MCP `session_close`, and TypeScript SDK `deleteSession` for session teardown.
- Add the `activatePage` primitive (MCP `page_activate`) to bring a page to the front on Chromium and Firefox.
- Add `GET /v1/events?stream=1` server-sent-event streaming with cursor frame ids and terminal gap frames.
- `GET /v1/mcp` now opens the streamable-HTTP SSE channel (keep-alive) instead of 405.
- Add `GET /v1/recovery/{workflow}`, MCP `recovery_status`, and TypeScript SDK `recoveryStatus` to inspect a workflow checkpoint and recovery receipts (`recovery:read`).
- Honor idempotency keys on session creation and checkpoint save, replaying retained results.
- Scope CDP-originated interface events to the authenticated principal.
- Report real uptime and in-flight command counts in runtime info.
- Add `listSessions` to the TypeScript SDK and stop rejecting checkpoints with recovery receipts.

### Packages

- Publish the TypeScript SDK as `@cavi-ai/bobby-browser` (replacing `@bobby-browser/sdk`).

### Semantic automation

- Add the `accessibilitySnapshot` primitive (MCP `a11y_snapshot`): a compact tree capped at 2048 nodes, from Chrome's full AX tree on Chromium and the companion extension's DOM walker on Firefox. Form controls include current value, description, required/disabled/read-only/invalid/checked state, autocomplete, and numeric bounds; sensitive values are redacted.
- Add command-ready semantic targets to actionable accessibility-snapshot nodes; duplicate role/name pairs receive deterministic tree-order ordinals without exposing DOM or browser IDs. Duplicate ordinals are computed on the full accessibility tree before `maxNodes` truncation, so retained targets keep globally correct ordinals.
- Carry snapshot targets into intents via `IntentHints.ordinal` and `intentHintsFromAccessibilityTarget`.
- Add verified `completeForm` intent (ordered uniquely named fill fields; no implicit submit).
- Add `FillValue` kind `checked` for reliable checkbox/radio semantic fills on Chromium and Firefox.
- Fill / completeForm verification fails closed on native HTML constraint validity (`required`, `pattern`, length, range, …) and retains the browser validation message in evidence.
- Add `expectedUrl` to `typeText` (all surfaces): typing fails before mutation when the page URL does not match, so agents cannot type into the wrong page.

### Extraction and vision

- Add the `extractStructured` primitive (MCP `extract_structured`): bounded page text plus the caller's JSON schema go to the configured provider, and the result is schema-validated and size-bounded before becoming `structuredExtraction` evidence. Gated on `browser:mutate`, `vision:assist`, session policy, and a configured provider.
- Plumb real screenshot bytes into vision escalation (`screenshot_bytes` on Chromium and Firefox workers); empty frames no longer reach providers.
- Add an HTTP vision-assist provider (`[vision]` config: https or loopback endpoint, bearer via env var) with response validation and fail-closed escalation.

### Firefox companion

- The Firefox native host treats a companion server silent for 45s as dead and reconnects, recovering from half-open connections left by killed processes.
- Recover stale Firefox companion attachments: a cycled companion connection now re-grants and retries once instead of failing every later action with `ConnectionClosed`; lease renewal re-grants dead attachments.
- Share one BiDi connection across runtime sessions on a Firefox profile (Firefox RemoteAgent accepts a single WebDriver session per browser).
- Keep prior attachment grants when issuing new ones, and renew attachment leases before expiry so sessions outlive the attachment TTL.
- Companion extension: merge attachment grants instead of replacing them, and retry terminal native-auth states after a bounded cooldown instead of stopping until a browser restart.
- Recover native-host descriptor publication from descriptor files leaked by killed processes.
- Log Firefox companion launch, pairing, and discovery failures as warnings.
- Add `bobby enroll-firefox-profile` for one-time Firefox companion pairing and selection output.
- Document Firefox companion setup and operations.

### CLI and startup

- Add `bobby doctor` setup checks and clap-based CLI help.
- Fail startup when the configured engine preference has no satisfiable worker registration.

## 0.2.1 - 2026-07-30

- Scope command outcome events to the authenticated principal across HTTP and MCP transports.
- Require session ownership for checkpoint creation and workflow recovery.
- Prevent workflow checkpoints from being rebound to a different session.
- Revalidate checkpoint session identity while holding the recovery lock.
- Default browser selection to exact Firefox without Chromium fallback.
- Bootstrap installed Firefox and its companion for championship runs.
- Support Playwright 1.62 bootstraps and repeated warmed client conformance runs.
