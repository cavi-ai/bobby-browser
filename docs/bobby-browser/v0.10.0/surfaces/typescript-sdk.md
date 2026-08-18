---
documentedVersion: 0.10.0
---

# TypeScript SDK

Package: `@cavi-ai/bobby-browser` (Node ≥ 22).

## Install

Published registry (when available):

```bash
npm install @cavi-ai/bobby-browser
```

From this monorepo after `pnpm install`:

```bash
pnpm --filter @cavi-ai/bobby-browser build
# import from the workspace package name in local packages/apps
```

## Construct the client

```ts
import { BrowserRuntimeClient } from "@cavi-ai/bobby-browser";

const client = new BrowserRuntimeClient({
  baseUrl: "http://127.0.0.1:7777",
  bearerToken: process.env.AUTOMATION_RUNTIME_TOKEN!,
});
```

Use `client.formSnapshot(sessionId, pageId, { maxControls })` for a read-only,
validated `FormSnapshot`. `maxControls` is optional and bounded from 1 through
512. The client calls the PageRead HTTP surface directly; it does not create a
mutating command envelope.

`baseUrl` should be the broker origin without a trailing `/v1` (the client
strips a trailing `/v1` if present). Bearer is the plaintext from `bobby init`
/ bootstrap (conventional env name `AUTOMATION_RUNTIME_TOKEN`).

## Headers

Every request sends:

- `Authorization: Bearer …`
- `x-interface-version: 2026-07-23` (`INTERFACE_VERSION`)
- `x-correlation-id` (UUID; override via `options.correlationId`)
- `x-deadline` (from `options.deadline` / `timeoutMs`, default 30s)

Pass `options.idempotencyKey` on mutating POSTs for replay-safe retries.

## Method catalog

| Method | HTTP | Notes |
|---|---|---|
| `runtimeInfo()` | `GET /v1/runtime` | |
| `createSession(input, options?)` | `POST /v1/sessions` | |
| `listSessions(options?)` | `GET /v1/sessions` | |
| `deleteSession(sessionId, options?)` | `DELETE /v1/sessions/{id}` | 204 on success |
| `openPage(input, options?)` | `POST /v1/pages` | |
| `submit(envelope, options?)` | `POST /v1/commands` | Includes primitives such as `activatePage` and `accessibilitySnapshot` |
| `checkpoint(input, options?)` | `POST /v1/checkpoints` | |
| `recoveryStatus(workflowId, options?)` | `GET /v1/recovery/{id}` | `RecoveryStatus` (`workflowId`, `checkpoint`, `receipts`) |
| `recover(workflowId, options?)` | `POST /v1/recovery/{id}` | |
| `events(cursor, options?)` | `GET /v1/events` | Async iterable over JSON batches (not `stream=1` SSE); handles `EventGap` |
| `artifact(reference, options?)` | `GET /v1/artifacts/{id}` | Verified byte stream |

There is no principals helper on the client today — mint/revoke with raw HTTP
(see [Authentication](../guides/auth.md)).

Use `controlActionRuntimeCommand(target, action)` to build the typed primitive
for `submit()`. The semantic target comes directly from `formSnapshot()`; no
CSS selector or test ID is accepted by this primitive.

## Intents

Build envelopes with helpers from the package (`locateEnvelope`,
`fillEnvelope`, `submitAndVerifyEnvelope`, `waitForStateEnvelope`,
`followEnvelope`, `dismissObstructionEnvelope`, `extractEnvelope`) and pass
them to `submit`. Over MCP, prefer the dedicated `intent_*` tools (same
semantics; server-minted envelopes). Multi-field verified forms use
`completeFormRuntimeCommand` plus `intentEnvelope`. `FillValue` kinds: `text`,
`select`, `checked`, `files`.
Use `intentHintsFromAccessibilityTarget(node.target)` to carry snapshot role,
accessible name, and duplicate-control ordinal into any intent. `TargetSpec`
fields are optional, so primitive commands can also accept the minimal
`{ role, accessibleName, ordinal? }` snapshot target without fabricated CSS
(HTTP/TS still require `selector: ""` beside `target`).
`AccessibilityNode` fields (form state + command-ready `target`):
[Accessibility snapshot](../guides/accessibility-snapshot.md).
Intent / vision details: [Intent commands](../guides/intents.md).
Structured extraction: MCP `extract_structured` / primitive `extractStructured`
(requires `vision:assist`).

## Errors

Failures throw `RuntimeClientError` with `kind` of `http` | `transport` |
`deadline` | `aborted` | `protocol`. HTTP interface errors expose
`interfaceError` (`InterfaceErrorCode` camelCase wire codes).

## Next

- [First browser session](../introduction/first-session.md)
- [HTTP API reference](http-api.md)
- [Authentication](../guides/auth.md)
