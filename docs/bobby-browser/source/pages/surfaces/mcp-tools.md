---
documentedVersion: {{PRODUCT_VERSION}}
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
| `session_close` | `session:write` | Close a session and release its worker |
| `session_list` | `session:read` | List sessions visible to the principal |
| `page_open` | `page:write` | Open a page in an owned session |
| `page_list` | `browser:mutate` | List pages in an owned session |
| `page_close` | `browser:mutate` | Close a page in an owned session |
| `page_activate` | `browser:mutate` | Bring a page to the front |
| `a11y_snapshot` | `browser:mutate` | Capture a compact accessibility tree with bounded form-control state, sensitive-value redaction, and command-ready semantic targets (`maxNodes` optional, 1…2048; default 256) |
| `navigate` | `browser:mutate` | Navigate a page to a URL |
| `click` | `browser:mutate` | Click an element |
| `type_text` | `browser:mutate` | Type text into an element |
| `inspect` | `browser:mutate` | Read page state, optionally element-scoped |
| `screenshot` | `browser:mutate` | Capture a screenshot artifact |
| `wait_for` | `browser:mutate` | Wait for a page condition |
| `download_url` | `browser:mutate` + `file:download` | Download a URL with digest evidence |
| `upload_files` | `browser:mutate` + `file:upload` | Set files on a file input |
| `evaluate_javascript` | `browser:mutate` + `javascript:evaluate` | Evaluate JavaScript (also session-policy gated) |
| `command_execute` | `browser:mutate` | Execute one bounded `CommandEnvelope` |
| `events_read` | `session:read` | Read retained events after a cursor |
| `checkpoint_save` | `recovery:write` | Persist a verified workflow checkpoint |
| `workflow_recover` | `recovery:write` | Recover a workflow from its verified checkpoint |

The flat browser tools (`navigate` … `evaluate_javascript`, plus
`page_activate` / `a11y_snapshot`) build the command envelope for you (ids and
deadline are server-generated) and return the same `CommandOutcome` shape as
`command_execute`, including artifact / accessibility evidence.

Intents and skills are **not** separate MCP tools. Submit intent command
envelopes only through `command_execute` (nested
`{ kind: "intent", input: { kind: "locate" \| … } }`). Nested capabilities such
as `intent:execute` still apply inside the runtime.

Compact accessibility trees (including form-control state):
[Accessibility snapshot](../guides/accessibility-snapshot.md).

Live JSON Schemas for tool arguments are defined in
`crates/mcp-gateway/src/schema.rs` (for example `session_create` requires
`profile`; `page_open` takes `sessionId`; `command_execute` takes `envelope`
and optional `idempotencyKey`). MCP argument names are camelCase even where
some HTTP request bodies use snake_case. The gateway's `schema_parity` tests
compare hand-bounded `kind` variant sets to schemars output from the
`types` crate (`schema` feature) so command/evidence drift fails CI.

## Lifecycle notes

- Token rotate / revoke → re-`initialize` on the MCP session for that principal
- Stdio startup uses the four `AUTOMATION_RUNTIME_BOOTSTRAP_*` variables, not
  `AUTOMATION_RUNTIME_TOKEN` alone
- HTTP MCP uses `Authorization: Bearer …` with the client bearer

## Next

- [First browser session](../introduction/first-session.md)
- [Intent commands](../guides/intents.md)
- [Events and recovery](../guides/events-recovery.md)
