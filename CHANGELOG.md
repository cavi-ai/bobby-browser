# Changelog

## Unreleased

- Fill / completeForm verification fails closed on native HTML constraint validity (`required`, `pattern`, length, range, …) and retains the browser validation message in evidence.
- Guard MCP schema parity with schemars: `JsonSchema` derives on the wire types and tests that fail when the hand-bounded MCP tool schemas drift from the Rust command/evidence variants.
>>>>>> ff26702 (test(schema): drift guards for new rebased types)
- Add verified `completeForm` intent (ordered uniquely named fill fields; no implicit submit).
- Add `FillValue` kind `checked` for reliable checkbox/radio semantic fills on Chromium and Firefox.
- Add the `accessibilitySnapshot` primitive (MCP `a11y_snapshot`): a compact `{role, name, children}` tree capped at 2048 nodes, from Chrome's full AX tree on Chromium and the companion extension's DOM walker on Firefox.
- Add `DELETE /v1/sessions/{id}`, MCP `session_close`, and TypeScript SDK `deleteSession` for session teardown.
- Add the `activatePage` primitive (MCP `page_activate`) to bring a page to the front on Chromium and Firefox.
- Add `GET /v1/events?stream=1` server-sent-event streaming with cursor frame ids and terminal gap frames.
- `GET /v1/mcp` now opens the streamable-HTTP SSE channel (keep-alive) instead of 405.
- The Firefox native host treats a companion server silent for 45s as dead and reconnects, recovering from half-open connections left by killed processes.

- Share one BiDi connection across runtime sessions on a Firefox profile (Firefox RemoteAgent accepts a single WebDriver session per browser).
- Keep prior attachment grants when issuing new ones, and renew attachment leases before expiry so sessions outlive the attachment TTL.
- Companion extension: merge attachment grants instead of replacing them, and retry terminal native-auth states after a bounded cooldown instead of stopping until a browser restart.
- Recover native-host descriptor publication from descriptor files leaked by killed processes.
- Log Firefox companion launch, pairing, and discovery failures as warnings.


## 0.3.0 - 2026-07-30

- Honor idempotency keys on session creation and checkpoint save, replaying retained results.
- Scope CDP-originated interface events to the authenticated principal.
- Report real uptime and in-flight command counts in runtime info.
- Add `listSessions` to the TypeScript SDK and stop rejecting checkpoints with recovery receipts.
- Fail startup when the configured engine preference has no satisfiable worker registration.
- Add `bobby doctor` setup checks and clap-based CLI help.
- Add `bobby enroll-firefox-profile` for one-time Firefox companion pairing and selection output.
- Document Firefox companion setup and operations.

## 0.2.1 - 2026-07-30

- Scope command outcome events to the authenticated principal across HTTP and MCP transports.
- Require session ownership for checkpoint creation and workflow recovery.
- Prevent workflow checkpoints from being rebound to a different session.
- Revalidate checkpoint session identity while holding the recovery lock.
- Default browser selection to exact Firefox without Chromium fallback.
- Bootstrap installed Firefox and its companion for championship runs.
- Support Playwright 1.62 bootstraps and repeated warmed client conformance runs.
