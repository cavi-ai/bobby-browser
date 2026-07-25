---
documentedVersion: 0.2.0
---

# Evidence and checkpoints

Adapters share evidence, checkpoint, and event contracts. Persist the last processed event cursor and reconnect with that cursor to resume exactly.

If retention has advanced, the adapter returns an `EventGap` with `historyLost` and `earliestAvailable`; restart from that cursor only after re-reading durable session/checkpoint state. Never guess across a gap.

Replayable work may retry only through runtime policy. Any loss at accepted, prepared, executing, verifying, or result-prepared boundaries that cannot prove the outcome remains `NeedsReconciliation`; it is never silently replayed.
