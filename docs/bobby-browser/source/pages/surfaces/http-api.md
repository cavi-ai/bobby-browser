---
documentedVersion: {{PRODUCT_VERSION}}
---

# HTTP API

`bobby serve` exposes a JSON API under `/v1`. Use it from any language, or through the [TypeScript](typescript-sdk.md), [Python](python-sdk.md) and [Rust](../rust/index.md) SDKs. The machine-readable catalog is the [OpenAPI 3.1 spec](../openapi/v1.yaml). Interface version: `{{INTERFACE_VERSION}}`.

`GET /healthz` needs no authentication and returns `{"ok": true}`. Every `/v1` route except `/v1/mcp` needs a bearer token and the headers in [Authentication](../guides/auth.md). Bodies use camelCase JSON unless noted.

## Routes

| Method | Path | Purpose | Capability |
|---|---|---|---|
| GET | `/v1/runtime` | Runtime info | `session:read` |
| GET | `/v1/sessions` | List sessions | `session:read` |
| POST | `/v1/sessions` | Create a session | `session:write` |
| DELETE | `/v1/sessions/{session}` | Delete a session (204) | `session:write` |
| POST | `/v1/pages` | Open a page | `page:write` |
| GET | `/v1/sessions/{session}/pages/{page}/forms` | Form snapshot | `page:read` |
| POST | `/v1/commands` | Submit a command envelope | `browser:mutate`, plus the command's own capability |
| GET | `/v1/context/ask` | Locate a described control | `context:read` |
| GET | `/v1/context/neighbors` | Remembered controls around a target | `context:read` |
| GET | `/v1/context/site/{key}` | Remembered structure for a site | `context:read` |
| POST | `/v1/checkpoints` | Save a checkpoint | `recovery:write` |
| GET | `/v1/recovery/{workflow}` | Checkpoint and receipts | `recovery:read` |
| POST | `/v1/recovery/{workflow}` | Recover a workflow | `recovery:write` |
| GET | `/v1/events` | Read events | `session:read` |
| GET | `/v1/artifacts/{id}` | Read artifact bytes | `artifact:read` |
| POST | `/v1/jobs` | Submit a job | `job:submit` (+ `network:egress` for HTTP jobs) |
| GET | `/v1/jobs/{job}` | Job status | `job:read` |
| DELETE | `/v1/jobs/{job}` | Cancel a job (204) | `job:cancel` |
| POST | `/v1/jobs/{job}/resolution` | Record an operator attestation | `job:read`, `job:cancel`, `authority:admin` |
| POST | `/v1/principals` | Issue a scoped bearer (201) | `authority:admin` |
| DELETE | `/v1/principals/{principal}` | Revoke a principal (204) | `authority:admin` |
| POST, GET | `/v1/mcp` | [MCP over HTTP](mcp-http.md) | per tool |

## Request details

**Create a session.** `POST /v1/sessions` takes `{profile, proxy, executionPolicy?, zigzagzig?}`. All `executionPolicy` flags default to off:

```json
{"profile": "default", "proxy": null, "executionPolicy": {"javascriptEvaluation": false, "visionAssist": false, "fingerprint": false, "humanize": false}}
```

`fingerprint` applies fingerprint spoofing and `humanize` adds human-like input timing. `zigzagzig: true` turns every flag on and recovers stuck commands automatically; see [Bobby skills](../guides/skills.md).

**Open a page.** `POST /v1/pages` takes `{session_id}`. This body is snake_case; session and page state also use `id`, `session_id` and `page_ids`.

**Submit a command.** `POST /v1/commands` takes an envelope with `schemaVersion: 2`, `commandId`, `workflowId`, `attemptId`, `sessionId`, `pageId`, `deadline`, and `command`. `command` is `{kind: "primitive" | "intent", input: {...}}`. Intents also need `intent:execute`. Examples are in [First session from code](../introduction/first-session.md) and [Intent commands](../guides/intents.md).

**Context reads.** `ask` and `neighbors` take query parameters `sessionId`, `pageId` and `description` (non-empty, up to 256 bytes). A hit returns `answer` or `neighbors` with `hit: true`. A miss returns `null`, `hit: false`, `reason: "notRemembered"` and `nextStep: "a11y_snapshot"`. Both set `pageDerived: true`. `context/site/{key}` returns `site` (or `null`) and `pageDerived: true`; percent-encode the key as one path segment.

**Form snapshot.** Optional query `maxControls` (1 to 512). Same contract as MCP `form_snapshot`.

**Checkpoints.** `POST /v1/checkpoints` takes `{checkpoint, evidenceRefs}`. `evidenceRefs` lists up to 128 command IDs the runtime has already journaled. The runtime resolves the evidence itself. An ID that this principal does not own, or that has no terminal record, fails the checkpoint.

**Recovery.** `GET` returns `{workflowId, checkpoint, receipts}` for a workflow you own. `POST` returns a recovery decision; `needsReconciliation` is HTTP 409.

**Events.** Query `after` (cursor) and `limit`. Add `stream=1` for server-sent events: each frame's `id` is its cursor, and a cursor gap ends the stream with an `event.gap` frame.

**Jobs.** `POST /v1/jobs` takes `{name, payload?, priority?, maxRetries?, timeoutMs?}` and returns `{jobId, status}`. `priority` is `low`, `normal` (default), `high` or `critical`; `maxRetries` defaults to 3. If persisting the job is uncertain, the error carries `jobId` and `reconciliationRequired: true`: query that job before submitting again. A job status with `reconciliationRequired` means the outcome is uncertain, so check the external effect first. `POST /v1/jobs/{job}/resolution` takes `{decision, evidenceSha256}` with `decision` of `effectObserved` or `effectAbsent` and a lowercase hex SHA-256. It records an operator's assertion, marks the job `resolved`, and never runs the handler.

**Principals.** `POST /v1/principals` takes `{principalId, capabilities, expiresAt}` and returns the bearer once.

## Idempotency

Send an `idempotency-key` on mutating POSTs. Keys are scoped to the principal and operation. A completed key can be replayed for 15 minutes. Unresolved keys stay in a durable ledger until the command or job reaches an outcome, and replays are re-authorized. Reusing a key with a different payload is `idempotencyConflict`.

## Errors

Failures return `{"error": {...}}` with `code`, `message`, `correlationId` and related fields.

| `code` | HTTP |
|---|---|
| `authenticationFailed`, `tokenExpired` | 401 |
| `missingCapability`, `malformedScope` | 403 |
| `artifactDenied`, `notFound` | 404 |
| `deadlineExceeded` | 408 |
| `idempotencyConflict`, reconciliation required | 409 |
| `invalidRequest`, `unsupportedInterfaceVersion` | 422 (413 when the body is too large) |
| `resourceExhausted` | 429 |
| `internal` | 500 |
| `engineUnreachable` | 503 |

`engineUnreachable` means the browser engine did not answer. Run `bobby doctor`, fix the engine it names, and resend the same request.

A command outcome maps to 200, 403, 409, 429 or 503 by its `status`. The SDKs check the status against the body.

### Rate limits

Each principal has an in-flight request limit (`interface.max_in_flight_per_principal`, default 8). When it is exceeded, and when command outcomes report `resourceExhausted`, the API returns 429 with `Retry-After` in whole seconds (rounded up, minimum 1) and `error.retryAfterMs` when a finer hint exists. Retry after the delay.

Retryable command failures return 503 with `Retry-After: 1`.

## Next

- [Authentication](../guides/auth.md)
- [Events and recovery](../guides/events-recovery.md)
- [MCP tools](mcp-tools.md)
