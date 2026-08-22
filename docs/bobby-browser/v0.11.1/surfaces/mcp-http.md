---
documentedVersion: 0.11.1
---

# MCP over streamable HTTP

The served runtime (`bobby serve`) exposes the same MCP tool surface over
streamable HTTP at `POST /v1/mcp` with bearer-only auth. Each tenant needs a
URL and its scoped token.

One JSON-RPC message per `POST`. `GET /v1/mcp` opens the streamable-HTTP
SSE channel, one JSON-RPC frame per `data:` line: this principal's runtime
events as `notifications/bobby/event`, plus `notifications/tools/list_changed`
on capability rotation. An idle stream emits a keep-alive comment every 15s.
A principal without `SubscribeEvents` gets the control frames only, never
event data. Open it with the same bearer as `POST`. Server state is isolated
per principal. A rotated or replaced bearer resets that principal's MCP
lifecycle — clients must `initialize` again.

## Required HTTP headers

MCP over HTTP is bearer-only. Unlike the rest of `/v1/*`, it takes no
`x-interface-version`, `x-correlation-id`, or `x-deadline` — MCP clients send
a static header set, so the route is mounted outside the strict-header
middleware. Sending them anyway is harmless; they are ignored.

| Header | Example |
|---|---|
| `Authorization` | `Bearer ${AUTOMATION_RUNTIME_TOKEN}` |
| `Content-Type` | `application/json` |

## Client config example

```json
{
  "mcpServers": {
    "bobby-browser": {
      "url": "http://127.0.0.1:7777/v1/mcp",
      "transport": "streamable-http",
      "headers": {
        "Authorization": "Bearer ${AUTOMATION_RUNTIME_TOKEN}"
      }
    }
  }
}
```

For a single-process local agent, [MCP stdio](mcp-stdio.md) may be simpler.

## curl smoke (`initialize`)

```bash
curl -sS http://127.0.0.1:7777/v1/mcp \
  -H "Authorization: Bearer ${AUTOMATION_RUNTIME_TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}'
```

## Lifecycle

Rotating or replacing the bearer resets that principal's MCP session state.
Clients must `initialize` again before tools. Tool catalog and capability
gates: [MCP tools](mcp-tools.md).

## Next

- Tool list and capabilities: [MCP tools](mcp-tools.md)
- Single-process local agent: [MCP stdio](mcp-stdio.md)
- NVIDIA OpenShell sandboxes: [OpenShell host](../guides/openshell.md)
- End-to-end loop: [First browser session](../introduction/first-session.md)
- Troubleshooting: [Troubleshooting](../guides/troubleshooting.md)
