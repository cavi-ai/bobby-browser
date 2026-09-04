---
documentedVersion: 0.13.0
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
TypeScript SDK `CheckpointRequest`: the checkpoint (workflow / attempt /
session / page ids, restart URL, recovery class, invariants, replayable inputs)
plus `evidenceRefs`.

`evidenceRefs` names command ids, not evidence. The runtime resolves each id
against the journal it wrote itself, checks that the naming principal owns the
command's session, and fails the checkpoint if an id has no terminal record. A
caller cannot author evidence for work it did not perform — the same contract
the MCP `checkpoint_save` tool enforces. Maximum 128 refs.

Typical moments to checkpoint:

- Before boundary work (`SubmitAndVerify`, boundary clicks /
  `Follow` with `boundary: true`) — required, not optional: the runtime
  refuses a Boundary command whose checkpoint does not already name the
  exact `commandId`/`attemptId` the command will carry.
- After durable evidence is available and before irreversible navigation

## `autoCheckpoint`

Over MCP, `intent_submit_and_verify`, `intent_follow`, and boundary `click`
accept `autoCheckpoint`, which **defaults to `true`**. The runtime mints the
pre-action checkpoint inside the same call and returns its `checkpointId`
alongside the usual `workflowId` / `commandId` / `attemptId`.

This is sugar over the gate, never a bypass. The runtime still matches the
checkpoint against the command on all five fields — workflow, attempt, session,
page, and `boundaryCommandId` — and a checkpoint that fails to save fails the
command rather than letting it run unprotected.

The runtime authors it because the gateway cannot: a checkpoint carries
`restartUrl` and `currentUrl`, and no interface method exposes live page state.

Pass `autoCheckpoint: false` when you need to author the checkpoint's
`invariants` or `replayableInputs`. Then pin `commandId`/`attemptId` yourself
and pass them to both `checkpoint_save` and the Boundary call; `intent_*` tools
and `click` accept both ids.

TypeScript:

```ts
await client.checkpoint(
  { checkpoint, evidenceRefs: [submitOutcome.commandId] },
  { idempotencyKey: crypto.randomUUID() },
);
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
