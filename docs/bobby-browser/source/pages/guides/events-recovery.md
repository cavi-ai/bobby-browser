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
arrives as a terminal `event.gap` frame.

The TypeScript SDK `events()` iterator reads **JSON batches**
(`GET /v1/events?after=&limit=`), not `stream=1`. Use raw SSE when you need a
push stream; use the SDK when batch polling and `EventGap` handling are enough.

## EventGap

If retention has advanced past the caller's cursor, the broker returns HTTP 409
with `{ error, gap }` where `gap` includes the earliest available cursor. The
SDK surfaces this as `RuntimeClientError` with `eventGap`. Restart from that
cursor only after re-reading durable session/checkpoint state. Never guess
across a gap.

`invalidCursor` and `invalidLimit` are caller errors.

## Recovery

Inspect durable state with `recovery:read`, then mutate with `recovery:write`.

```ts
const status = await client.recoveryStatus(workflowId);
// status.workflowId, status.checkpoint, status.receipts

const decision = await client.recover(workflowId, {
  idempotencyKey: crypto.randomUUID(),
});
```

| Surface | Inspect (`recovery:read`) | Mutate (`recovery:write`) |
|---|---|---|
| HTTP | `GET /v1/recovery/{workflowId}` | `POST /v1/checkpoints`, `POST /v1/recovery/{workflowId}` |
| MCP | `recovery_status` (`{ workflowId }`) | `checkpoint_save`, `workflow_recover` |
| TypeScript SDK | `recoveryStatus(workflowId)` | `checkpoint(…)`, `recover(workflowId)` |

`GET` / `recovery_status` returns camelCase `RecoveryStatus`:
`{ workflowId, checkpoint, receipts }`. The workflow must be owned by the
caller; missing or unowned workflows return not found. `receipts` mirrors the
durable recovery receipts bound to the checkpoint.

Any loss at accepted, prepared, executing, verifying, or result-prepared
boundaries that cannot prove the outcome remains `NeedsReconciliation`.

Skill-assisted recovery (internal skill runtime) follows the same authority
rules — see [Internal skill runtime](skills.md). Not the public agent skill.

## Next

- [Evidence and checkpoints](../concepts/evidence-checkpoints.md)
- [HTTP API](../surfaces/http-api.md)
- [Troubleshooting](troubleshooting.md)
