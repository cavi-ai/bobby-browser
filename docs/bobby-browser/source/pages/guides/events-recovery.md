---
documentedVersion: {{PRODUCT_VERSION}}
---

# Events and recovery

Read the runtime's event stream to follow what happened, and use checkpoints to resume a workflow after a crash or interruption. For what checkpoints contain, see [Evidence and checkpoints](../concepts/evidence-checkpoints.md).

## Read events

`GET /v1/events?after=<cursor>&limit=<n>` returns a batch. It needs `session:read`. `limit` is capped by `interface.max_event_batch` (default 256).

```ts
for await (const event of client.events(0, { limit: 100 })) {
  console.log(event.cursor, event);
}
```

Store the last cursor you processed and resume from it. Over MCP, `events_read` takes `cursor` and `limit` and waits for a newer event or its deadline. `notifications/bobby/event` pushes the same events.

For a push stream over HTTP, add `stream=1`. Each server-sent frame has the event's cursor as its `id`.

### Gaps

If retention has moved past your cursor, the API returns HTTP 409 with the earliest available cursor. A stream ends with an `event.gap` frame. The TypeScript client throws `RuntimeClientError` with `eventGap`. Re-read durable state (sessions, checkpoints), then resume from the earliest cursor. Do not guess across a gap. `invalidCursor` and `invalidLimit` are caller errors.

## Save a checkpoint

A checkpoint records where a workflow is, backed by evidence the runtime already journaled. Pass the IDs of completed commands as `evidenceRefs`; the runtime resolves the evidence itself.

```ts
await client.checkpoint({ checkpoint, evidenceRefs: [commandId] }, { idempotencyKey: crypto.randomUUID() });
```

Over MCP, boundary tools can checkpoint for you with `autoCheckpoint`.

## Inspect and recover

| Surface | Inspect (`recovery:read`) | Recover (`recovery:write`) |
|---|---|---|
| HTTP | `GET /v1/recovery/{workflowId}` | `POST /v1/checkpoints`, `POST /v1/recovery/{workflowId}` |
| MCP | `recovery_status` | `checkpoint_save`, `workflow_recover` |
| TypeScript | `recoveryStatus(workflowId)` | `checkpoint(...)`, `recover(workflowId)` |

`recovery_status` returns `{workflowId, checkpoint, receipts}` for a workflow you own. Pass `sessionId` instead of `workflowId` over MCP to list the session's recoverable workflows, newest first.

```ts
const status = await client.recoveryStatus(workflowId);
const decision = await client.recover(workflowId, { idempotencyKey: crypto.randomUUID() });
```

A recovery decision is to resume, restart, or reconcile. When bobby cannot prove whether an interrupted action took effect, the decision is `needsReconciliation` (HTTP 409). Check the page, then continue. Never repeat the action blindly.

## Optional follow-up observations

Some MCP actions return a follow-up observation as `postStateStatus`: `available`, `unavailable` or `notRequested`. If it is `unavailable`, the action itself still succeeded and `postState` is omitted. Read fresh state before the next step. Do not repeat a successful action because the observation failed.

## Storage problems and uncertain jobs

`GET /v1/runtime` reports `storageIntegrity` when durable history is unreadable, has duplicate keys, or is too large to restore. `bobby doctor` reports the same. Reads keep working, and new mutations that depend on the damaged store are refused. Restarting does not clear it. Keep the damaged files and repair them before restarting, because deleting the ledger and resubmitting can repeat an effect.

For a job whose outcome is uncertain, an operator with `authority:admin` can record what they observed with `POST /v1/jobs/{job}/resolution`, or `resolveJob` and `resolve_job` in the SDKs. If a resolution write fails, further attestations for that job are refused until history is reloaded.

## Next

- [Evidence and checkpoints](../concepts/evidence-checkpoints.md)
- [HTTP API](../surfaces/http-api.md)
- [Troubleshooting](troubleshooting.md)
