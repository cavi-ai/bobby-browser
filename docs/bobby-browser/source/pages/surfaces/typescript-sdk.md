---
documentedVersion: 0.3.0
---

# TypeScript SDK

Package: `@bobby-browser/sdk` (Node ≥ 22).

## Install

Published registry (when available):

```bash
npm install @bobby-browser/sdk
```

From this monorepo after `pnpm install`:

```bash
pnpm --filter @bobby-browser/sdk build
# import from the workspace package name in local packages/apps
```

## Construct the client

```ts
import { BrowserRuntimeClient } from "@bobby-browser/sdk";

const client = new BrowserRuntimeClient({
  baseUrl: "http://127.0.0.1:7777",
  bearerToken: process.env.AUTOMATION_RUNTIME_TOKEN!,
});
```

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
| `recover(workflowId, options?)` | `POST /v1/recovery/{id}` | |
| `events(cursor, options?)` | `GET /v1/events` | Async iterable; handles `EventGap` |
| `artifact(reference, options?)` | `GET /v1/artifacts/{id}` | Verified byte stream |

There is no principals helper on the client today — mint/revoke with raw HTTP
(see [Authentication](../guides/auth.md)).

## Intents

Build envelopes with helpers from the package (`locateEnvelope`,
`fillEnvelope`, `submitAndVerifyEnvelope`, `waitForStateEnvelope`,
`followEnvelope`, `dismissObstructionEnvelope`, `extractEnvelope`) and pass
them to `submit`. Multi-field verified forms use `completeFormRuntimeCommand`
plus `intentEnvelope`. `FillValue` kinds: `text`, `select`, `checked`, `files`.
`AccessibilityNode` form-state fields on a11y snapshots:
[Accessibility snapshot](../guides/accessibility-snapshot.md).
Intent details: [Intent commands](../guides/intents.md).

## Errors

Failures throw `RuntimeClientError` with `kind` of `http` | `transport` |
`deadline` | `aborted` | `protocol`. HTTP interface errors expose
`interfaceError` (`InterfaceErrorCode` camelCase wire codes).

## Next

- [First browser session](../introduction/first-session.md)
- [HTTP API reference](http-api.md)
- [Authentication](../guides/auth.md)
