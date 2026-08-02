---
documentedVersion: {{PRODUCT_VERSION}}
---

# HTTP API reference

Authenticated broker routes under `/v1/*`. Interface version:
`{{INTERFACE_VERSION}}` (`CURRENT_INTERFACE_VERSION` / TypeScript `INTERFACE_VERSION`).

Unauthenticated: `GET /healthz` → `{ "ok": true }`.

There is no `/runtime` route. Use `GET /v1/runtime`.

Shared headers for every `/v1/*` call: [Authentication](../guides/auth.md).

## Routes

| Method | Path | Purpose | Required capability |
|---|---|---|---|
| GET | `/v1/runtime` | Runtime info | `session:read` |
| GET | `/v1/sessions` | List sessions | `session:read` |
| POST | `/v1/sessions` | Create session | `session:write` |
| DELETE | `/v1/sessions/{sessionId}` | Delete session (204) | `session:write` |
| POST | `/v1/pages` | Open page | `page:write` |
| POST | `/v1/commands` | Submit command envelope | `browser:mutate` (+ nested caps for upload / download / JS / intents) |
| POST | `/v1/checkpoints` | Persist workflow checkpoint | `recovery:write` |
| POST | `/v1/recovery/{workflow}` | Recover workflow | `recovery:write` |
| GET | `/v1/recovery/{workflow}` | Checkpoint + receipts | `recovery:read` |
| GET | `/v1/events` | Read events (`after`, `limit` query) | `session:read` |
| GET | `/v1/artifacts/{id}` | Read artifact bytes | `artifact:read` |
| POST | `/v1/principals` | Issue scoped bearer | `authority:admin` |
| DELETE | `/v1/principals/{principal}` | Revoke principal | `authority:admin` |

MCP streamable HTTP is mounted at `POST /v1/mcp` (JSON-RPC) and
`GET /v1/mcp` (SSE keep-alive channel) — see [MCP over HTTP](mcp-http.md)
and [MCP tools](mcp-tools.md).

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
- **DELETE `/v1/sessions/{sessionId}`** — empty body; `204` on success
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

## Clients

- Typed client: [TypeScript SDK](typescript-sdk.md)
- Rust HTTP client: [bobby-browser-client](../rust/bobby-browser-client.md)
- [MCP tools](mcp-tools.md)
- Compact a11y trees: [Accessibility snapshot](../guides/accessibility-snapshot.md)
- Tutorial: [First browser session](../introduction/first-session.md)
- Headers and mint curl: [Authentication](../guides/auth.md)
