---
documentedVersion: 0.2.0
---

# Events and recovery

This page owns **event cursors and `EventGap`**. Durable checkpoints and
reconciliation ownership live in
[Evidence and checkpoints](../concepts/evidence-checkpoints.md).

## Reading events

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
const decision = await client.recover(workflowId, { idempotencyKey: crypto.randomUUID() });
```

Skill-assisted recovery (internal skill runtime / gauntlet) follows the same
authority rules — see [Bobby skills](skills.md). Public HTTP/MCP clients use
`checkpoint` + `recover` only.
