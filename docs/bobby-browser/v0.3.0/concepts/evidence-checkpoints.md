---
documentedVersion: 0.3.0
---

# Evidence and checkpoints

Adapters share evidence, checkpoint, and recovery contracts. This page owns
**durable checkpoints and reconciliation**. Cursor continuity and `EventGap`
live in [Events and recovery](../guides/events-recovery.md).

## Checkpoints

`POST /v1/checkpoints` (`recovery:write`) persists a verified
`WorkflowCheckpoint` with evidence. Checkpoints bind workflow / attempt /
session / page identity, restart URL, recovery class, invariants, and replayable
inputs.

Use checkpoints before boundary work (`SubmitAndVerify`, boundary clicks /
`Follow` with `boundary: true`). Loss at accepted, prepared, executing,
verifying, or result-prepared boundaries that cannot prove the outcome remains
`NeedsReconciliation` — never silently replayed.

## Recovery

`POST /v1/recovery/{workflowId}` returns a `RecoveryDecision`. The TypeScript
client maps `needsReconciliation` to HTTP 409.

Replayable work may retry only through runtime policy. Boundary / reconciliable
classes follow command-class rules — see [Intent commands](../guides/intents.md).

## Evidence

Command outcomes carry typed evidence items (navigation, DOM snapshots,
screenshots, JavaScript results, …). Artifact bytes are fetched separately with
`artifact:read` via `GET /v1/artifacts/{id}` / `client.artifact(reference)`.

Cross-link: [Events and recovery](../guides/events-recovery.md) for event
cursors and gap handling.
