---
documentedVersion: 0.2.0
---

# MCP tools reference

Both [MCP stdio](mcp-stdio.md) (`mcp-gateway`) and [MCP over HTTP](mcp-http.md)
(`POST /v1/mcp`) expose the same tool surface after a successful `initialize`.

## Protocol

- MCP protocol version: `2025-11-25`
- Call `initialize` before any tool listing or tool call
- Streamable HTTP: one JSON-RPC message per `POST` (no `GET` transport)
- Tool argument validation is bounded (stdio: ~1 MiB frames, 256 KiB tool input,
  event reads capped at 256 records)

## Tools

Tools are advertised only when the principal holds the required capability.

| Tool | Required capability | Purpose |
|---|---|---|
| `runtime_info` | `session:read` | Runtime capability and health information |
| `session_create` | `session:write` | Create a browser session |
| `session_list` | `session:read` | List sessions visible to the principal |
| `page_open` | `page:write` | Open a page in an owned session |
| `command_execute` | `browser:mutate` | Execute one bounded `CommandEnvelope` |
| `events_read` | `session:read` | Read retained events after a cursor |
| `checkpoint_save` | `recovery:write` | Persist a verified workflow checkpoint |
| `workflow_recover` | `recovery:write` | Recover a workflow from its verified checkpoint |

Intents and skills are **not** separate MCP tools. Submit intent command
envelopes only through `command_execute` (nested
`{ kind: "intent", input: { kind: "locate" \| … } }`). Nested capabilities such
as `intent:execute` still apply inside the runtime.

Live JSON Schemas for tool arguments are defined in
`crates/mcp-gateway/src/schema.rs` (for example `session_create` requires
`profile`; `page_open` takes `sessionId`; `command_execute` takes `envelope`
and optional `idempotencyKey`). MCP argument names are camelCase even where
some HTTP request bodies use snake_case.

Files under `schemas/mcp/tools/` in this repository use older names
(`session.create`, `page.goto`, …) and do **not** match the eight tools above —
do not treat those filenames as the current public MCP catalog.

## Lifecycle notes

- Token rotate / revoke → re-`initialize` on the MCP session for that principal
- Stdio startup uses the four `AUTOMATION_RUNTIME_BOOTSTRAP_*` variables, not
  `AUTOMATION_RUNTIME_TOKEN` alone
- HTTP MCP uses `Authorization: Bearer …` with the client bearer

## Next

- [First browser session](../introduction/first-session.md)
- [Intent commands](../guides/intents.md)
- [Events and recovery](../guides/events-recovery.md)
