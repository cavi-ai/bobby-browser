---
documentedVersion: 0.5.1
---

# First browser session

End-to-end path from a cold checkout to one navigate command. Prefer the
TypeScript SDK for application code; the MCP path mirrors the same operations
for agent hosts.

## 1. Bootstrap and serve

```bash
cargo build -p bobby-browser
./target/debug/bobby init
# copy the printed bearer, then:
export AUTOMATION_RUNTIME_TOKEN='…plaintext bearer…'
./target/debug/bobby serve
```

Confirm liveness: `curl -sS http://127.0.0.1:7777/healthz` → `{"ok":true}`.

Authenticated routes live under `/v1/*` only (for example `GET /v1/runtime`).
See [Authentication](../guides/auth.md).

### Optional: raw HTTP smoke (`GET /v1/runtime`)

```bash
CORRELATION=$(uuidgen | tr '[:upper:]' '[:lower:]')
DEADLINE=$(date -u -v+60S +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d '+60 seconds' +%Y-%m-%dT%H:%M:%SZ)
curl -sS http://127.0.0.1:7777/v1/runtime \
  -H "Authorization: Bearer ${AUTOMATION_RUNTIME_TOKEN}" \
  -H "x-interface-version: 2026-07-23" \
  -H "x-correlation-id: ${CORRELATION}" \
  -H "x-deadline: ${DEADLINE}"
```

## 2. TypeScript: session → page → navigate

Install the published package, or use the workspace package from this repo:

```bash
npm install @cavi-ai/bobby-browser
# workspace:
# pnpm --filter @cavi-ai/bobby-browser…  (from monorepo root after pnpm install)
```

```ts
import { BrowserRuntimeClient } from "@cavi-ai/bobby-browser";
import { randomUUID } from "node:crypto";

const client = new BrowserRuntimeClient({
  baseUrl: "http://127.0.0.1:7777",
  bearerToken: process.env.AUTOMATION_RUNTIME_TOKEN!,
});

const session = await client.createSession(
  { profile: "default", proxy: null, executionPolicy: { javascriptEvaluation: false, visionAssist: false, fingerprint: false, humanize: false } },
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

// Optional: compact accessibility tree (primitive accessibilitySnapshot)
await client.submit(
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
      input: { kind: "accessibilitySnapshot", input: { maxNodes: 256 } },
    },
  },
  { idempotencyKey: randomUUID() },
);

// Optional: bring a background page forward (primitive activatePage)
await client.submit(
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
      input: { kind: "activatePage", input: { pageId: page.id } },
    },
  },
  { idempotencyKey: randomUUID() },
);

await client.deleteSession(session.id);
```

For goal-oriented steps, use intent helpers (`locateEnvelope`, …) from
`@cavi-ai/bobby-browser` — they still submit via `client.submit` and need
`intent:execute` (included in default bootstrap). See [Intent commands](../guides/intents.md).

## 3. Parallel MCP path

With `bobby serve` running, point an MCP client at streamable HTTP
(`POST /v1/mcp`) with `Authorization: Bearer ${AUTOMATION_RUNTIME_TOKEN}`, or
run `mcp-gateway` stdio with the four bootstrap env vars
([MCP stdio](../surfaces/mcp-stdio.md)).

Order:

1. `initialize` (protocol `2025-11-25`) — required before tools
2. `tools/call` → `session_create` with `{ "profile": "default" }`
3. `tools/call` → `page_open` with `{ "sessionId": "…", "url": "https://…" }`
4. Prefer flat tools: `navigate`, `a11y_snapshot`, `click` / `type_text` /
   `upload_files` (selector or snapshot `target`), and `intent_*` for
   goal-oriented steps — they mint the envelope server-side
5. Use `command_execute` when you need a nested primitive / intent envelope
   the flat tools do not cover
6. Optionally `events_read` / `checkpoint_save` / `workflow_recover` (pass a
   returned `workflowId` to keep continuity)

Full catalog: [MCP tools](../surfaces/mcp-tools.md). Intents:
[Intent commands](../guides/intents.md).

## 4. Outcome and recovery

- Success statuses include `completed` (and related terminal outcomes on the wire).
- Policy / capability failures return interface errors such as `missingCapability`.
- After interruption, persist checkpoints and call recover — see
  [Events and recovery](../guides/events-recovery.md).
- Auth and path mistakes: [Authentication](../guides/auth.md),
  [HTTP API](../surfaces/http-api.md).

Next: [TypeScript SDK](../surfaces/typescript-sdk.md) ·
[Rust HTTP client](../rust/bobby-browser-client.md) ·
[HTTP API](../surfaces/http-api.md) · [Quickstart](quickstart.md)
