---
documentedVersion: 0.2.0
---

# Events and recovery

Persist the last processed event cursor. Reconnect with that cursor to resume exactly. If retention has advanced, the adapter returns an `EventGap` with `historyLost` and `earliestAvailable`; restart from that cursor only after re-reading durable session/checkpoint state.

`invalidCursor` and `invalidLimit` are caller errors. Never guess across a gap. Loss at accepted, prepared, executing, verifying, or result-prepared boundaries that cannot prove the outcome remains `NeedsReconciliation`.

Skill-assisted recovery follows the same authority rules. A recovery receipt starts unresolved, binds one exact command and decision, and advances forward-only to its terminal result. Bobby replays a committed receipt instead of repeating browser effects after interruption. See [Bobby skills](skills.md) for the bounded tactic ladder.
