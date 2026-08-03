---
documentedVersion: 0.4.0
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
| `session/request_permission` (agent → client) | Vision-escalation approval only |

## Structured prompts

A prompt is a single text block of JSON — an optional `url` plus one intent in
the exact shape `command_execute` accepts. There is no planner and no
freeform natural language:

```json
{"url": "https://example.com/form", "intent": {"kind": "locate", "input": {"purpose": "the submit button"}}}
```

`url` is opened and navigated first, then the intent runs. Outcomes stream
back as `session/update` agent-message chunks; the turn ends with
`endTurn` (completed), `refusal` (failed, needs-reconciliation, or denied),
or `cancelled`.

## Permission prompts cannot mint authority

`session/request_permission` fires only for vision escalation, and only when
the principal already holds `vision:assist` while the session's
`executionPolicy.visionAssist` is off — the exact double gate the other
surfaces enforce. Approval lifts the *session gate* by rerunning in a session
created with the flag on; the capability was the principal's all along. A
principal without `vision:assist` is denied without any prompt: there is no
button a human can click that creates authority the token never carried.
