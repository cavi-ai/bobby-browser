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

MCP command results include `verificationStatus`. The SDKs classify the same
typed evidence with Rust's `CommandOutcome::verification_status()`, TypeScript's
`commandVerificationStatus(outcome)`, and Python's `command_verification_status(outcome)`.
HTTP command responses retain their existing shape.

| Status | What the evidence establishes |
|---|---|
| `notCompleted` | The command did not return `completed`; inspect its outcome and error. |
| `notReported` | No wait or submit-settlement evidence was reported. A plain click may have completed. |
| `conditionObserved` | A recorded wait condition was observed. Read that condition to determine what it proves. |
| `settled` | Submission settled without reported client-side validation rejection. Server acceptance is not established. |
| `validationRejected` | The submission reported client-side validation rejection. Repair the form before an explicit resubmission. |

These classifications do not authorize retries. Diagnostic strings and optional
follow-up observations never promote an action to a confirmed business result.

Some MCP actions return a follow-up observation as `postStateStatus`: `available`, `unavailable` or `notRequested`. This describes the read, separately from the action's `status`. A landed submit with failed verification may return fresh `postState` for recovery while remaining failed. If the read is `unavailable`, `postState` is omitted and the action's outcome is unchanged. Inspect the diagnostic before reading fresh state; do not repeat a landed action because its observation or verification failed.

## Storage problems and uncertain jobs

`GET /v1/runtime` reports `storageIntegrity` when durable history is unreadable, has duplicate keys, or is too large to restore. `bobby doctor` reports the same. Reads keep working, and new mutations that depend on the damaged store are refused. Restarting does not clear it. Keep the damaged files and repair them before restarting, because deleting the ledger and resubmitting can repeat an effect.

For damaged command, job, or idempotency history:

1. Save `bobby doctor --json` and the affected store names and reasons.
2. Pause callers and stop Bobby before inspecting files offline.
3. Preserve the affected ledger and its backups, including unresolved reservations.
4. Establish whether affected operations took effect using authoritative application state before repairing history or retrying an operation.

`bobby doctor --fix` repairs supported setup problems, such as missing storage
directories. It does not repair damaged execution history or resolve uncertain
effects. If authoritative state cannot establish an outcome, preserve that
uncertainty; deleting reservations can permit a duplicate effect.

For a job whose outcome is uncertain, an operator with `authority:admin` can record what they observed with `POST /v1/jobs/{job}/resolution`, or `resolveJob` and `resolve_job` in the SDKs. If a resolution write fails, further attestations for that job are refused until history is reloaded.

## Export an incident report

```bash
bobby doctor --config /path/to/config.toml --incident-bundle /path/to/new-incident.json
```

The destination parent must exist. The export creates a new local JSON file
(with mode `0600` on Unix);
it refuses existing destinations, storage-source and lock paths, and checkpoint
or artifact directories. It cannot be combined with `--fix`, downgrade, or other
doctor output modes. No browser or health endpoint is contacted.

The report contains the runtime version, collection time, health observations for
the command and scheduler journals and their three idempotency ledgers, streamed
SHA-256 hashes, checkpoint/artifact directory presence, and recovery prerequisites.
It includes local paths but omits configuration contents, credentials, record
payloads, screenshots, checkpoints, and artifact contents.

An export exit code of zero means the report was written, **not** that history is
healthy. Each store has its own status: `healthy`, `needsRepair`, `missing`, `busy`,
`inspectionFailed`, `unsupportedFileType`, `changedDuringCollection`, or
`unavailable`. Busy or changed sources have no hash. Collection is a non-atomic
observation, not a coordinated snapshot or backup. No status or hash grants
permission to restore or replay work. Preserve original files and backups, stop
Bobby before offline repair, and establish uncertain effects from authoritative
application state before choosing a recovery candidate.

## Next

- [Evidence and checkpoints](../concepts/evidence-checkpoints.md)
- [HTTP API](../surfaces/http-api.md)
- [Troubleshooting](troubleshooting.md)
