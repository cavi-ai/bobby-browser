---
documentedVersion: {{PRODUCT_VERSION}}
---

# Evidence and checkpoints

Adapters share evidence, checkpoint, and recovery contracts. This page owns
**durable checkpoints and reconciliation**. Cursor continuity and `EventGap`
live in [Events and recovery](../guides/events-recovery.md).

## Why checkpoints

Long workflows must survive worker restarts and process crashes without silently
replaying side effects. A checkpoint is a verified snapshot of workflow
identity, page/session binding, restart URL, recovery class, invariants, and
replayable inputs.

## Writing a checkpoint

`POST /v1/checkpoints` requires `recovery:write`. Body shape matches the
TypeScript SDK `CheckpointRequest` (workflow / attempt / session / page ids plus
verified evidence).

Typical moments to checkpoint:

- Before boundary work (`SubmitAndVerify`, boundary clicks /
  `Follow` with `boundary: true`)
- After durable evidence is available and before irreversible navigation

TypeScript:

```ts
await client.checkpoint(request, { idempotencyKey: crypto.randomUUID() });
```

## Recovery

Inspect with `GET /v1/recovery/{workflowId}` / `client.recoveryStatus` /
MCP `recovery_status` (`recovery:read`) before or after mutate calls.

`POST /v1/recovery/{workflowId}` returns a `RecoveryDecision`. The TypeScript
client maps `needsReconciliation` to HTTP 409.

Loss at accepted, prepared, executing, verifying, or result-prepared boundaries
that cannot prove the outcome remains `NeedsReconciliation` — never silently
replayed. Replayable work may retry only through runtime policy. Boundary /
reconciliable classes follow command-class rules — see
[Intent commands](../guides/intents.md).

Details and surface matrix: [Events and recovery](../guides/events-recovery.md).

## Evidence

Command outcomes carry typed evidence items (navigation, DOM snapshots,
screenshots, JavaScript results, …). Artifact bytes are fetched separately with
`artifact:read` via `GET /v1/artifacts/{id}` / `client.artifact(reference)`.

Do not treat screenshots or JS results in the outcome envelope as a substitute
for a durable checkpoint when you need restart safety.

## Next

- [Events and recovery](../guides/events-recovery.md)
- [Intent commands](../guides/intents.md)
- [TypeScript SDK](../surfaces/typescript-sdk.md)
