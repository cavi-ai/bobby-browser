---
documentedVersion: {{PRODUCT_VERSION}}
---

# Events and recovery

This page owns **event cursors and `EventGap`**. Durable checkpoints and
reconciliation ownership live in
[Evidence and checkpoints](../concepts/evidence-checkpoints.md).

## Reading events (batch)

`GET /v1/events?after=<cursor>&limit=<n>` requires `session:read`.
`limit` is bounded by interface config (`max_event_batch`, default 256).

TypeScript:

```ts
for await (const event of client.events(0, { limit: 100 })) {
  // event.cursor advances; persist the last processed cursor
  console.log(event);
}
```

Persist the last processed event cursor. Reconnect with that cursor to resume
exactly.

## Reading events (SSE)

Pass `stream=1` for a server-sent-event stream instead of a JSON batch:

```http
GET /v1/events?after=0&limit=100&stream=1
Authorization: Bearer …
x-interface-version: {{INTERFACE_VERSION}}
x-correlation-id: …
x-deadline: …
```

Each event arrives as an SSE frame whose `id` is its cursor. A retention gap
arrives as a terminal `event.gap` frame. Prefer the TypeScript SDK iterator
unless you need raw SSE.

## EventGap

If retention has advanced past the caller's cursor, the broker returns HTTP 409
with `{ error, gap }` where `gap` includes the earliest available cursor. The
SDK surfaces this as `RuntimeClientError` with `eventGap`. Restart from that
cursor only after re-reading durable session/checkpoint state. Never guess
across a gap.

`invalidCursor` and `invalidLimit` are caller errors.

## Recovery

Any loss at accepted, prepared, executing, verifying, or result-prepared
boundaries that cannot prove the outcome remains `NeedsReconciliation`.

```ts
const decision = await client.recover(workflowId, {
  idempotencyKey: crypto.randomUUID(),
});
```

Skill-assisted recovery (internal skill runtime / gauntlet) follows the same
authority rules — see [Bobby skills](skills.md). Public HTTP/MCP clients use
`checkpoint` + `recover` only.

## Next

- [Evidence and checkpoints](../concepts/evidence-checkpoints.md)
- [HTTP API](../surfaces/http-api.md)
- [Troubleshooting](troubleshooting.md)
