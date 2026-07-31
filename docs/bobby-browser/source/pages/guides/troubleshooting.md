---
documentedVersion: 0.3.0
---

# Troubleshooting

Run `bobby doctor` first when something local fails to start. Use
`--config` / `--bootstrap-env` to match how you launch `serve`. See
[CLI reference](cli.md).

## Auth failures (`401`)

- Missing or wrong `Authorization: Bearer …`
- Expired or revoked principal
- Using bootstrap **file** vars incorrectly for HTTP clients — export the
  plaintext as `AUTOMATION_RUNTIME_TOKEN` for the SDK / curl
- MCP **stdio** needs all four `AUTOMATION_RUNTIME_BOOTSTRAP_*` vars (not token alone)
- Non-loopback `serve` without a bootstrap credential — run `bobby init` first

See [Authentication](auth.md).

## `missingCapability` (`403`)

Principal lacks the capability for that operation (or nested command). Check the
matrix in [Capabilities](../concepts/capabilities.md). Default `bobby init`
includes a full set; scoped issued principals do not.

## Wrong path (`404`)

Use `/v1/runtime`, not `/runtime`. Catalog: [HTTP API](../surfaces/http-api.md).
Session delete is `DELETE /v1/sessions/{sessionId}` (not a POST).

## `EventGap` (`409` on events)

Retention advanced past your cursor. Re-read durable state, then resume from
`earliestAvailable` — [Events and recovery](events-recovery.md).

## SSE / streaming events

`GET /v1/events?after=…&limit=…&stream=1` is the SSE path. If your client cannot
parse SSE frames, omit `stream` and use the batch JSON response instead.

## MCP initialize order

Call `initialize` (protocol `2025-11-25`) before `tools/list` or `tools/call`.
After token rotate on HTTP MCP, re-initialize for that principal.
MCP HTTP still needs the broker context headers
(`x-interface-version`, `x-correlation-id`, `x-deadline`) —
[MCP over HTTP](../surfaces/mcp-http.md).

## Browser / engine

- Default engine preference is **Firefox**. If doctor warns on Firefox BiDi,
  start Firefox with remote debugging and set
  `AUTOMATION_RUNTIME_BROWSER_SELECTION` (see [Firefox companion](firefox-companion.md)).
- Chromium live work needs an installed Chromium. Set
  `BOBBY_CHROMIUM_EXECUTABLE` when not in a standard location.
- Gauntlet / championship tests are often `--ignored` until a browser is present —
  [Browser gauntlet](gauntlet.md).

## Config and bootstrap paths

- Malformed `config.toml` fails startup and prints the path — fix TOML, do not
  put secrets in the file.
- `BOBBY_BROWSER_CONFIG` / `bobby serve --config` select the file.
- `BOBBY_BROWSER_BOOTSTRAP_ENV` / `bobby serve --bootstrap-env` select the secret
  dotenv from `bobby init`.

## Interface version

Send `x-interface-version: 2026-07-23`. Mismatch →
`unsupportedInterfaceVersion`.

## Session lifecycle

- Create: `POST /v1/sessions` / `session_create`
- Delete / close: `DELETE /v1/sessions/{id}` / MCP `session_close` /
  TypeScript `deleteSession`
- Bring a page forward: primitive `activatePage` or MCP `page_activate`

## Semantic fill failures

- Prefer exact `nearText` + `role` when the accessible name is known; leave
  `purpose` as the agent task phrase.
- `kind: "select"` matches option **value**, not visible label.
- `kind: "checked"` is only for checkbox/radio. Radios accept
  `checked: true` only — unchecking a radio fails closed.
- A fill without postcondition evidence fails; do not treat a silent click as
  success. Re-locate and retry under a new attempt id / idempotency key.
- Files need `file:upload` on the bearer — missing capability →
  `missingCapability`.
- `completeForm` stops at the first failed field; evidence includes prior
  successful fields plus the failing field. Fix that field (or hints), then
  resubmit the whole form intent under a new attempt / idempotency key.
  Duplicate or empty field `name`s are rejected before dispatch.
- Native HTML constraints: if evidence has `formControlValid: "false"`, read
  `formControlValidationMessage` and correct that value. Do not treat a
  committed DOM value as success when constraint validity failed.

## Accessibility snapshot

- Primitive `accessibilitySnapshot` / MCP `a11y_snapshot` needs only
  `browser:mutate`.
- Default `maxNodes` is 256 (clamp 1…2048). Large pages set `truncated: true`
  — raise `maxNodes` or narrow the viewport / DOM before retrying.
- Form-control nodes may include `value`, `required`, `invalid`, `checked`,
  bounds, and related flags. Password / masked values appear as
  `"[redacted]"`.
- Guide: [Accessibility snapshot](accessibility-snapshot.md).

## Error catalog (`InterfaceErrorCode`)

| Code | Meaning |
|---|---|
| `invalidRequest` | Malformed headers/body/query |
| `unsupportedInterfaceVersion` | Bad or missing interface version |
| `invalidIdempotencyKey` | Idempotency key shape/bounds |
| `idempotencyConflict` | Same key, different payload |
| `deadlineExceeded` | Past `x-deadline` |
| `authenticationFailed` | Bad/missing bearer |
| `tokenExpired` | Principal expired |
| `missingCapability` | Capability check failed |
| `malformedScope` | Scope/authority malformed |
| `artifactDenied` | Artifact access denied |
| `unsupportedOperation` | Operation not supported |
| `notFound` | Missing resource |
| `resourceExhausted` | Capacity / in-flight limits |
| `internal` | Unexpected server failure |
