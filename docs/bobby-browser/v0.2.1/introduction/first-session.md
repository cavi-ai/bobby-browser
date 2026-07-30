---
documentedVersion: 0.2.1
---

# First browser session

End-to-end path from a cold checkout to one navigate command. Prefer the
TypeScript SDK for application code; the MCP path mirrors the same operations
for agent hosts.

## 1. Bootstrap and serve

```bash
cargo build -p cli
./target/debug/bobby init
# copy the printed bearer, then:
export AUTOMATION_RUNTIME_TOKEN='…plaintext bearer…'
./target/debug/bobby serve
```

Confirm liveness: `curl -sS http://127.0.0.1:7777/healthz` → `{"ok":true}`.

Authenticated routes live under `/v1/*` only (for example `GET /v1/runtime`).
See [Authentication](../guides/auth.md).

## 2. TypeScript: session → page → navigate

Install the published package, or use the workspace package from this repo:

```bash
npm install @bobby-browser/sdk
# workspace:
# pnpm --filter @bobby-browser/sdk…  (from monorepo root after pnpm install)
```

```ts
import { BrowserRuntimeClient } from "@bobby-browser/sdk";
import { randomUUID } from "node:crypto";

const client = new BrowserRuntimeClient({
  baseUrl: "http://127.0.0.1:7777",
  bearerToken: process.env.AUTOMATION_RUNTIME_TOKEN!,
});

const session = await client.createSession(
  { profile: "default", proxy: null, executionPolicy: { javascriptEvaluation: false, visionAssist: false } },
  { idempotencyKey: randomUUID() },
);

const page = await client.openPage(
  { session_id: session.id },
  { idempotencyKey: randomUUID() },
);

const deadline = new Date(Date.now() + 60_000).toISOString();
const outcome = await client.submit(
  {
    schemaVersion: 2,
    commandId: randomUUID(),
    workflowId: randomUUID(),
    attemptId: randomUUID(),
    sessionId: session.id,
    pageId: page.id,
    deadline,
    command: {
      kind: "primitive",
      input: {
        kind: "navigate",
        input: {
          url: "https://example.com",
          waitUntil: "domContentLoaded",
          timeoutMs: 30_000,
        },
      },
    },
  },
  { idempotencyKey: randomUUID() },
);

console.log(outcome.status, outcome);
```

For goal-oriented steps, use intent helpers (`locateEnvelope`, …) from
`@bobby-browser/sdk` — they still submit via `client.submit` and need
`intent:execute` (included in default bootstrap). See [Intent commands](../guides/intents.md).

## 3. Parallel MCP path

With `bobby serve` running, point an MCP client at streamable HTTP
(`POST /v1/mcp`) with `Authorization: Bearer ${AUTOMATION_RUNTIME_TOKEN}`, or
run `mcp-gateway` stdio with the four bootstrap env vars
([MCP stdio](../surfaces/mcp-stdio.md)).

Order:

1. `initialize` (protocol `2025-11-25`) — required before tools
2. `tools/call` → `session_create` with `{ "profile": "default" }`
3. `tools/call` → `page_open` with `{ "sessionId": "…" }`
4. `tools/call` → `command_execute` with a `CommandEnvelope` (same shape as TS)
5. Optionally `events_read` / `checkpoint_save` / `workflow_recover`

There are no dedicated intent MCP tools; intents go only through
`command_execute`. Full catalog: [MCP tools](../surfaces/mcp-tools.md).

## 4. Outcome and recovery

- Success statuses include `completed` (and related terminal outcomes on the wire).
- Policy / capability failures return interface errors such as `missingCapability`.
- After interruption, persist checkpoints and call recover — see
  [Events and recovery](../guides/events-recovery.md).
- Auth and path mistakes: [Authentication](../guides/auth.md),
  [HTTP API](../surfaces/http-api.md).

Next: [TypeScript SDK](../surfaces/typescript-sdk.md) · [HTTP API](../surfaces/http-api.md) · [Quickstart](quickstart.md)
