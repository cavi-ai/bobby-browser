---
documentedVersion: 0.2.1
---

# Troubleshooting

## Auth failures (`401`)

- Missing or wrong `Authorization: Bearer …`
- Expired or revoked principal
- Using bootstrap **file** vars incorrectly for HTTP clients — export the
  plaintext as `AUTOMATION_RUNTIME_TOKEN` for the SDK
- MCP **stdio** needs all four `AUTOMATION_RUNTIME_BOOTSTRAP_*` vars (not token alone)

See [Authentication](auth.md).

## `missingCapability` (`403`)

Principal lacks the capability for that operation (or nested command). Check the
matrix in [Capabilities](../concepts/capabilities.md). Default `bobby init`
includes a full set; scoped issued principals do not.

## Wrong path (`404`)

Use `/v1/runtime`, not `/runtime`. Catalog: [HTTP API](../surfaces/http-api.md).

## `EventGap` (`409` on events)

Retention advanced past your cursor. Re-read durable state, then resume from
`earliestAvailable` — [Events and recovery](events-recovery.md).

## MCP initialize order

Call `initialize` (protocol `2025-11-25`) before `tools/list` or `tools/call`.
After token rotate on HTTP MCP, re-initialize for that principal.

## Chromium missing

Live browser work needs an installed Chromium. Set
`BOBBY_CHROMIUM_EXECUTABLE` when not in a standard location. Gauntlet /
championship tests are often `--ignored` until a browser is present — see
[Browser gauntlet](gauntlet.md).

## Interface version

Send `x-interface-version: 2026-07-23`. Mismatch →
`unsupportedInterfaceVersion`.

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
