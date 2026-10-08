---
documentedVersion: {{PRODUCT_VERSION}}
---

# First session from code

Open a page and read it through the HTTP API and the TypeScript SDK. For the agent path, see the [Quickstart](quickstart.md).

## Start the server

```bash
bobby serve
export AUTOMATION_RUNTIME_TOKEN="$(bobby token)"
curl http://127.0.0.1:7777/healthz
```

`/healthz` returns `{"ok":true}` without authentication. All other routes live under `/v1/` and need a bearer token and the headers described in [Authentication](../guides/auth.md).

## Call the API with curl

```bash
DEADLINE=$(date -u -v+60S +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d '+60 seconds' +%Y-%m-%dT%H:%M:%SZ)
curl -sS http://127.0.0.1:7777/v1/runtime \
  -H "Authorization: Bearer ${AUTOMATION_RUNTIME_TOKEN}" \
  -H "x-interface-version: {{INTERFACE_VERSION}}" \
  -H "x-correlation-id: $(uuidgen | tr 'A-Z' 'a-z')" \
  -H "x-deadline: ${DEADLINE}"
```

## Navigate with the TypeScript SDK

```ts
import { randomUUID } from "node:crypto";
import { BrowserRuntimeClient } from "@cavi-ai/bobby-browser";

const client = new BrowserRuntimeClient({
  baseUrl: "http://127.0.0.1:7777",
  bearerToken: process.env.AUTOMATION_RUNTIME_TOKEN!,
});

const session = await client.createSession(
  {
    profile: "default",
    proxy: null,
    executionPolicy: {
      javascriptEvaluation: false,
      visionAssist: false,
      fingerprint: false,
      humanize: false,
    },
  },
  { idempotencyKey: randomUUID() },
);
const page = await client.openPage(
  { session_id: session.id },
  { idempotencyKey: randomUUID() },
);

const outcome = await client.submit(
  {
    schemaVersion: 2,
    commandId: randomUUID(),
    workflowId: randomUUID(),
    attemptId: randomUUID(),
    sessionId: session.id,
    pageId: page.id,
    deadline: new Date(Date.now() + 60_000).toISOString(),
    command: {
      kind: "primitive",
      input: {
        kind: "navigate",
        input: { url: "https://example.com", waitUntil: "domContentLoaded", timeoutMs: 30_000 },
      },
    },
  },
  { idempotencyKey: randomUUID() },
);
console.log(outcome.status);

await client.deleteSession(session.id);
```

The client sets the interface, correlation, deadline and authorization headers. Pass an `idempotencyKey` on every call that changes state.

Each command is an envelope with a `kind` of `primitive` or `intent`. Intent helpers in the SDK build the envelope for goal-level steps. See [Intent commands](../guides/intents.md).

## Retry safely

An idempotency key is written to a durable ledger before the browser acts, so it survives a gateway crash. If the gateway dies mid-submit and you resend the same key, the runtime returns `idempotencyConflict` with `reconciliationRequired: true` instead of repeating the action. Check the page or call `recovery_status`, then continue from what you find. Do not mint a new key. See [Events and recovery](../guides/events-recovery.md).

## Next

- [HTTP API](../surfaces/http-api.md)
- [TypeScript SDK](../surfaces/typescript-sdk.md), [Python SDK](../surfaces/python-sdk.md), [Rust SDK](../rust/index.md)
- [MCP over HTTP](../surfaces/mcp-http.md)
