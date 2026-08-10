---
documentedVersion: 0.8.0
---

# HTTP API reference

Authenticated broker routes under `/v1/*`. Interface version:
`2026-07-23` (`CURRENT_INTERFACE_VERSION` / TypeScript `INTERFACE_VERSION`).

Unauthenticated: `GET /healthz` → `{ "ok": true }`.

There is no `/runtime` route. Use `GET /v1/runtime`.

Shared headers for every `/v1/*` call: [Authentication](../guides/auth.md).

## Routes

| Method | Path | Purpose | Required capability |
|---|---|---|---|
| GET | `/v1/runtime` | Runtime info | `session:read` |
| GET | `/v1/sessions` | List sessions | `session:read` |
| POST | `/v1/sessions` | Create session | `session:write` |
| DELETE | `/v1/sessions/{session}` | Delete session (204) | `session:write` |
| POST | `/v1/pages` | Open page | `page:write` |
| POST | `/v1/commands` | Submit command envelope | `browser:mutate` (+ nested caps for upload / download / JS / intents) |
| POST | `/v1/checkpoints` | Persist workflow checkpoint | `recovery:write` |
| POST | `/v1/recovery/{workflow}` | Recover workflow | `recovery:write` |
| GET | `/v1/recovery/{workflow}` | Checkpoint + receipts | `recovery:read` |
| GET | `/v1/events` | Read events (`after`, `limit` query) | `session:read` |
| GET | `/v1/artifacts/{id}` | Read artifact bytes | `artifact:read` |
| GET | `/v1/sessions/{session}/pages/{page}/forms` | Form snapshot (`maxControls` query, 1–512) | `page:read` |
| POST | `/v1/jobs` | Submit a scheduled job | `job:submit` |
| GET | `/v1/jobs/{job}` | Job status | `job:read` |
| DELETE | `/v1/jobs/{job}` | Cancel a job | `job:cancel` |
| POST | `/v1/principals` | Issue scoped bearer | `authority:admin` |
| DELETE | `/v1/principals/{principal}` | Revoke principal | `authority:admin` |

MCP streamable HTTP is mounted at `POST /v1/mcp` (JSON-RPC) and
`GET /v1/mcp` (SSE keep-alive channel) — see [MCP over HTTP](mcp-http.md)
and [MCP tools](mcp-tools.md).

Machine-readable catalog: [OpenAPI 3.1](../openapi/v1.yaml).

## Request bodies (high level)

Shapes use camelCase JSON. Do not invent fields; follow the TypeScript SDK
validators / Rust types.

- **POST `/v1/sessions`** — `{ profile, proxy, executionPolicy? }`. Every
  `executionPolicy` flag is deny-by-default, so an omitted policy is
  `{ javascriptEvaluation: false, visionAssist: false, fingerprint: false, humanize: false }`.
  `fingerprint` applies fingerprint spoofing to workers leased for this
  session; `humanize` synthesizes human-like input timing and reports what it
  synthesized as `humanization` evidence. Both are written to the worker on
  every lease, so one session's opt-in never carries into another's.
- **DELETE `/v1/sessions/{session}`** — empty body; `204` on success
- **POST `/v1/pages`** — `{ session_id }` (snake_case on this request; session/page state also uses `id` / `session_id` / `page_ids`)
- **POST `/v1/commands`** — `CommandEnvelope` (`schemaVersion: 2`, ids, `deadline`,
  `command` where `command` is `{ kind: "primitive"|"intent", input: … }`).
  Primitive `activatePage` uses `{ kind: "activatePage", input: { pageId } }`.
  Primitive `accessibilitySnapshot` uses
  `{ kind: "accessibilitySnapshot", input: { maxNodes? } }` (default 256,
  max 2048; see [Accessibility snapshot](../guides/accessibility-snapshot.md)).
- **POST `/v1/checkpoints`** — `{ checkpoint, evidenceRefs }` (see SDK
  `CheckpointRequest`). `evidenceRefs` is a bounded list (max 128) of command
  ids whose evidence the runtime already journaled; it resolves them itself.
  Evidence is never supplied by the caller, and an id naming a command this
  principal does not own, or one with no terminal journal record, fails the
  checkpoint.
- **GET `/v1/recovery/{workflow}`** — `RecoveryStatus`
  (`{ workflowId, checkpoint, receipts }`); requires `recovery:read` and session
  ownership of the workflow. Missing / unowned → not found.
- **POST `/v1/recovery/{workflow}`** — returns `RecoveryDecision`; maps
  `needsReconciliation` to HTTP 409
- **POST `/v1/principals`** — `{ principalId, capabilities, expiresAt }` → `201` with one-time `bearer`
- **GET `/v1/events`** — query `after` (cursor) and `limit` (bounded; SDK max 256).
  Pass `stream=1` for a server-sent-event stream instead of a batch: each event
  arrives as an SSE frame whose `id` is its cursor, a cursor gap arrives as a
  terminal `event.gap` frame.
- **GET `/v1/sessions/{session}/pages/{page}/forms`** — optional query
  `maxControls` (integer 1–512). Returns a `FormSnapshot` (same contract as MCP
  `form_snapshot`).
- **POST `/v1/jobs`** — `{ name, payload?, priority?, maxRetries?, timeoutMs? }`
  → `{ jobId, status }`. `priority` is `low` | `normal` | `high` | `critical`
  (default `normal`); `maxRetries` defaults to `3`. Mutating: send
  `idempotency-key` for safe retries.
- **GET `/v1/jobs/{job}`** — job status record (`id`, `name`, `priority`,
  `status`, `payload`, timestamps, `retryCount`, `maxRetries`, `result`,
  `error`, …).
- **DELETE `/v1/jobs/{job}`** — cancel; returns the updated job status.

Nested command kinds include primitives (`navigate`, `click`, …) and
`{ kind: "intent", input: … }`. Intents additionally need `intent:execute`.

## Status and errors

Successful JSON responses are typically `200`. Principal issuance returns
`201`. Session delete and principal revocation return `204`.

Failures return JSON `{ "error": { … } }` where `error` is an `InterfaceError`:

| `code` (camelCase) | Typical HTTP |
|---|---|
| `authenticationFailed` / `tokenExpired` | 401 |
| `missingCapability` / `malformedScope` | 403 |
| `artifactDenied` / `notFound` | 404 |
| `deadlineExceeded` | 408 |
| `idempotencyConflict` / reconciliation | 409 |
| `resourceExhausted` | 429 |
| `invalidRequest` | 422 (or 413 when oversized) |
| `unsupportedInterfaceVersion` | 422 |
| `internal` | 500 |

Command outcomes may map to `200` / `403` / `409` / `429` / `503` depending on
`CommandOutcome.status` — the TypeScript client checks status against the body.

### Rate limits and retry

Each principal has an independent in-flight request quota
(`interface.max_in_flight_per_principal` — see
[Multi-principal runtime](../concepts/multi-principal.md)). Exhaustion and
command `resourceExhausted` outcomes return **HTTP 429** with:

- Response header `Retry-After: <seconds>` (integer seconds)
- Body field `error.retryAfterMs` when the runtime supplies a millisecond hint

`Retry-After` is always whole seconds: the broker rounds millisecond hints
**up** (`ceil(ms / 1000)`, minimum **1**). A 50 ms hint therefore yields
`Retry-After: 1`. Prefer `retryAfterMs` in the body when you need sub-second
intent; never treat a missing or zero header as “retry immediately.”

Treat 429 as retryable after the indicated delay. Do not spin. Connection /
accept limits on the listener can also emit 429 with `Retry-After`.

HTTP **503** appears for retryable command failures (`retryableFailure`). Those
responses always include `Retry-After: 1` — a fixed default, because
`retryableFailure` has no per-outcome millisecond field yet. Prefer that
header over spinning.

## Clients

- Typed client: [TypeScript SDK](typescript-sdk.md)
- Rust HTTP client: [bobby-browser-client](../rust/bobby-browser-client.md)
- [MCP tools](mcp-tools.md)
- Compact a11y trees: [Accessibility snapshot](../guides/accessibility-snapshot.md)
- Tutorial: [First browser session](../introduction/first-session.md)
- Headers and mint curl: [Authentication](../guides/auth.md)
