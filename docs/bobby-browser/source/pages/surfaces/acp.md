---
documentedVersion: {{PRODUCT_VERSION}}
---

# ACP (Agent Client Protocol)

`bobby acp-stdio` lets an ACP editor, such as Zed, drive bobby over stdio. It follows the same capability, idempotency, evidence, checkpoint and event rules as the HTTP and MCP surfaces. For editor setup, see [Zed over ACP](../guides/acp-zed.md).

## Connect

```bash
bobby install --host acp --yes
```

This writes a project `.acp.json` that launches `bobby acp-stdio`. The command loads the bootstrap credential and attaches to the scope's shared runtime, so the host configuration holds no secrets. The `acp-gateway` binary reads the four `AUTOMATION_RUNTIME_BOOTSTRAP_*` variables directly if you launch it yourself. Add `--team` and `--project` for a scoped runtime.

The protocol is ACP schema version 1.

## Methods

| ACP method | Behavior |
|---|---|
| `initialize` | Handshake and agent capabilities |
| `session/new` | Creates a runtime session. The ACP session ID is the runtime session ID |
| `session/prompt` | Runs one structured request |
| `session/cancel` | Interrupts the running prompt, including permission waits |
| `session/close` | Cancels active work, deletes the session and frees its browser |

Only one prompt runs per session at a time. If the editor disconnects, the gateway closes its sessions. ACP sessions count against `browser.max_active`.

## Prompts

A prompt is one text block of JSON. There is no planner and no natural-language parsing.

To run a command, send an optional `url`, an optional `workflowId`, and one intent in the shape `command_execute` accepts:

```json
{"url": "https://example.com/form", "intent": {"kind": "locate", "input": {"purpose": "the submit button"}}}
```

The first `url` opens a page and later URLs navigate that page, so cookies and state persist. The reply has `operation: "execute"`, the IDs, and the full command outcome with evidence. The turn ends with `endTurn` (completed), `refusal` (failed, denied or needs reconciliation) or `cancelled`.

Other operations:

| `operation` | Fields | Does |
|---|---|---|
| `contextAsk` | `description` | Look up a control in retained context |
| `contextNeighbors` | `description` | Read the remembered form around a control |
| `contextSite` | `siteKey` | Read remembered structure for a site |
| `checkpointSave` | `checkpoint`, `evidenceRefs` | Save a checkpoint from evidence the runtime already holds |
| `recoveryStatus` | optional `workflowId`, `limit` | Read one workflow, or list the session's workflows |
| `workflowRecover` | `workflowId` | Recover a workflow |

Each returns one JSON `session/update` chunk with `operation` and `result`. Context results match MCP and HTTP: `{"answer": ..., "hit": true, "pageDerived": true}`, or on a miss `hit: false`, `reason: "notRemembered"`, `nextStep: "a11y_snapshot"`.

## Permission prompts

`session/request_permission` is sent only for vision escalation, when the caller holds `vision:assist` but the session's `executionPolicy.visionAssist` is off. Approval covers that command's retry on the existing page and does not change the session. A caller without `vision:assist` is denied without a prompt.
