---
documentedVersion: {{PRODUCT_VERSION}}
---

# Evidence and checkpoints

Every command returns typed evidence of what happened. A checkpoint records where a workflow stands, so it can resume after a crash without repeating side effects. Event cursors and gaps are covered in [Events and recovery](../guides/events-recovery.md).

## Evidence

A command outcome carries evidence items such as navigation results, accessibility snapshots, screenshots, JavaScript results and download digests. Fetch artifact bytes with `artifact:read` through `GET /v1/artifacts/{id}`, `client.artifact(reference)`, or an `artifact://<id>` MCP resource. Evidence in an outcome is not a substitute for a checkpoint when you need restart safety.

## Checkpoints

A checkpoint holds the workflow, attempt, session and page IDs, the restart URL, a recovery class, invariants and replayable inputs. `POST /v1/checkpoints` and the MCP tool `checkpoint_save` need `recovery:write`.

```ts
await client.checkpoint(
  { checkpoint, evidenceRefs: [submitOutcome.commandId] },
  { idempotencyKey: crypto.randomUUID() },
);
```

`evidenceRefs` lists up to 128 command IDs, not evidence. The runtime resolves each against its own journal and checks that you own the command's session. A checkpoint fails if an ID has no terminal record, so a caller cannot author evidence for work it did not do.

Checkpoint before boundary work: `intent_submit_and_verify`, `intent_follow` with `boundary: true`, and boundary clicks. The runtime refuses a boundary command unless a checkpoint already names its exact `commandId` and `attemptId`.

### `autoCheckpoint`

Over MCP, boundary tools take `autoCheckpoint`, which defaults to `true`. The runtime saves the checkpoint inside the same call and returns its `checkpointId`. The checkpoint must still match the command on workflow, attempt, session, page and boundary command. If it cannot be saved, the command fails instead of running unprotected.

Pass `autoCheckpoint: false` to author `invariants` or `replayableInputs` yourself. Then pin `commandId` and `attemptId` and pass the same IDs to `checkpoint_save` and the boundary call.

## Recovery

`GET /v1/recovery/{workflowId}` (or `recovery_status`) returns the checkpoint and receipts. `POST /v1/recovery/{workflowId}` (or `workflow_recover`) returns a decision to resume, restart or reconcile. If bobby cannot prove whether an interrupted action took effect, the decision is `needsReconciliation` (HTTP 409) and nothing is replayed. Replayable work retries only under runtime policy. Command classes are in [Intent commands](../guides/intents.md).

## Audit bundles

`bobby audit export --workflow <workflowId>` writes a tar that a reviewer can check offline:

| File | Contents |
|---|---|
| `journal.jsonl` | The workflow's command journal lines, byte for byte |
| `checkpoint.json` | The workflow's checkpoint, if any |
| `artifacts/<id>/...` | Artifacts the journal names that are still on disk. Missing ones are listed as `missingArtifacts` |
| `manifest.json` | SHA-256 and size of every file |
| `signature.json` | Ed25519 signature over the manifest |

The signing key is created on first use in the config directory (`audit-signing-key.pk8`, owner-only). `bobby audit key` prints the public key. `bobby audit verify <bundle> --public-key <hex>` recomputes every digest, rejects missing, extra or altered files, and checks the signature against that key. Without `--public-key` it accepts any valid signer and says so. Export reads the runtime's files and runs beside a live runtime. See [Workflow replay](../guides/replay.md).

## Next

- [Events and recovery](../guides/events-recovery.md)
- [Intent commands](../guides/intents.md)
