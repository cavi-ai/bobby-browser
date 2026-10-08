---
documentedVersion: {{PRODUCT_VERSION}}
---

# TypeScript SDK

`@cavi-ai/bobby-browser` is a typed client for the [HTTP API](http-api.md). It requires Node 22 or later.

```bash
npm install @cavi-ai/bobby-browser
```

## Connect

```ts
import { BrowserRuntimeClient } from "@cavi-ai/bobby-browser";

const client = new BrowserRuntimeClient({
  baseUrl: "http://127.0.0.1:7777",
  bearerToken: process.env.AUTOMATION_RUNTIME_TOKEN!,
});
const info = await client.runtimeInfo();
```

`baseUrl` is the server origin. A trailing `/v1` is accepted. Get the token with `bobby token`.

The client sends the authorization, interface version, correlation ID and deadline headers on every request. Per-call options:

| Option | Effect |
|---|---|
| `idempotencyKey` | Replay-safe retries for mutating calls |
| `correlationId` | Override the generated UUID |
| `deadline`, `timeoutMs` | Request deadline (default 30 seconds) |

Responses are limited to 64 MiB before parsing. Raise it up to 256 MiB with the `maxJsonResponseBytes` constructor option. Artifact reads have their own `maxArtifactBytes` limit (64 MiB default, up to 256 MiB).

## Methods

| Method | Route |
|---|---|
| `runtimeInfo()` | `GET /v1/runtime` |
| `createSession(input, options?)` | `POST /v1/sessions` |
| `listSessions()` | `GET /v1/sessions` |
| `deleteSession(sessionId)` | `DELETE /v1/sessions/{id}` |
| `openPage(input, options?)` | `POST /v1/pages` |
| `formSnapshot(sessionId, pageId, {maxControls}?)` | `GET /v1/sessions/{session}/pages/{page}/forms` |
| `submit(envelope, options?)` | `POST /v1/commands` |
| `contextAsk`, `contextNeighbors`, `contextSite` | `GET /v1/context/*` |
| `checkpoint(input, options?)` | `POST /v1/checkpoints` |
| `recoveryStatus(workflowId)` | `GET /v1/recovery/{id}` |
| `recover(workflowId)` | `POST /v1/recovery/{id}` |
| `events(cursor, options?)` | `GET /v1/events`. Async iterable over batches; resumes after an event gap |
| `artifact(reference)` | `GET /v1/artifacts/{id}`. Verified byte stream |
| `submitJob`, `jobStatus`, `cancelJob` | `/v1/jobs` |
| `resolveJob(jobId, input)` | `POST /v1/jobs/{job}/resolution` |

Issue and revoke principals with the HTTP API; see [Authentication](../guides/auth.md).

`resolveJob` records an operator attestation for a job whose outcome is uncertain. `input` is `{decision: "effectObserved" | "effectAbsent", evidenceSha256}`. The caller needs `job:read`, `job:cancel` and `authority:admin`. See [Events and recovery](../guides/events-recovery.md).

## Intents

Intent helpers build the command for `submit`:

| Helper | Intent |
|---|---|
| `locateEnvelope` | Find a control |
| `fillEnvelope` | Fill one control |
| `submitAndVerifyEnvelope` | Submit and verify |
| `followEnvelope` | Activate and verify |
| `waitForStateEnvelope` | Wait for a state |
| `dismissObstructionEnvelope` | Dismiss an overlay |
| `extractEnvelope` | Read named fields |
| `detectChallengeEnvelope`, `solveChallengeEnvelope` | Challenge detection and solving |

For several fields at once, combine `completeFormRuntimeCommand` with `intentEnvelope`. `controlActionRuntimeCommand(target, action)` builds a native control action from a `formSnapshot` target. Use `intentHintsFromAccessibilityTarget(node.target)` to carry a snapshot's role, name and ordinal into an intent. See [Intent commands](../guides/intents.md).

## Errors

Failures throw `RuntimeClientError` with `kind` set to `http`, `transport`, `deadline`, `aborted` or `protocol`. HTTP errors expose `interfaceError` with the wire `code`.

## Next

- [First session from code](../introduction/first-session.md)
- [HTTP API](http-api.md)
- [Python SDK](python-sdk.md)
