---
documentedVersion: 0.11.0
---

# ACP (Agent Client Protocol)

`acp-gateway` lets an ACP-speaking editor drive the runtime over stdio. It is
the fourth adapter, held to the same capability, idempotency, evidence,
checkpoint, and event contracts as [HTTP](http-api.md),
[MCP stdio](mcp-stdio.md), and [CDP](cdp.md) by
`crates/interface-conformance`.

## Build and start

```bash
cargo build -p acp-gateway --release
# binary: ./target/release/acp-gateway
```

Or install the host fragment with the CLI (copies `acp-gateway` when built
alongside `bobby`, writes project `.acp.json` pointing at `bobby acp-stdio`):

```bash
bobby install --host acp --cli --yes
```

`bobby acp-stdio` loads the bootstrap credential the same way `bobby mcp-stdio`
does, then execs `acp-gateway`. No bootstrap env vars belong in the host
config file.

Startup takes the same four `AUTOMATION_RUNTIME_BOOTSTRAP_*` variables as
`bobby init` writes; missing or invalid input fails closed. Protocol version
pinned: ACP schema v1 (`agent-client-protocol` 2.x).

## Wire scope (v1)

| ACP | Runtime mapping |
|---|---|
| `initialize` | Protocol handshake; advertises agent capabilities |
| `session/new` | Runtime session (the ACP session id *is* the runtime session id) |
| `session/prompt` | One structured automation request, streamed back as `session/update` chunks |
| `session/cancel` | Cancels the in-flight prompt step |
| `session/close` | Cancels active work, deletes the runtime session, and releases browser capacity |
| `session/request_permission` (agent → client) | Vision-escalation approval only |

## Structured prompts

A prompt is a single text block of JSON — an optional `url` plus one intent in
the exact shape `command_execute` accepts. There is no planner and no
freeform natural language:

```json
{"url": "https://example.com/form", "intent": {"kind": "locate", "input": {"purpose": "the submit button"}}}
```

The first `url` opens a page; later URLs navigate that same page so cookies,
storage, and live page state remain in one browser context without accumulating
orphan pages. Outcomes stream back as `session/update` agent-message chunks;
the turn ends with `endTurn` (completed), `refusal` (failed,
needs-reconciliation, or denied), or `cancelled`.

Only one prompt turn may run per session. `session/cancel` interrupts browser
work, permission waits, and post-approval retries. `session/close` waits for
that cancellation to settle before deleting the runtime session. If the editor
disconnects without closing its sessions, the gateway performs the same cleanup.
ACP sessions share the runtime-wide `browser.max_active` capacity bound; close
or disconnect cleanup releases both the browser worker and ownership slot.

## Permission prompts cannot mint authority

`session/request_permission` fires only for vision escalation, and only when
the principal already holds `vision:assist` while the session's
`executionPolicy.visionAssist` is off — the exact double gate the other
surfaces enforce. Approval applies only to that command's retry on the existing
page; it neither changes the stored session policy nor returns a reusable
vision-enabled session. The capability was the principal's all along. A
principal without `vision:assist` is denied without any prompt: there is no
button a human can click that creates authority the token never carried.
