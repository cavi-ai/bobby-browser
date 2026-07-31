# Changelog

## Unreleased

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
