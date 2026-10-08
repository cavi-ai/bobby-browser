---
documentedVersion: {{PRODUCT_VERSION}}
---

# MCP over HTTP

`bobby serve` exposes the MCP tools at `POST /v1/mcp` for clients that connect by URL, such as sandboxes and remote agents. Tools are listed in [MCP tools](mcp-tools.md). For a local agent host, [MCP stdio](mcp-stdio.md) is simpler.

## Connect

Authentication is a bearer token only. This route does not take `x-interface-version`, `x-correlation-id` or `x-deadline`.

```json
{
  "mcpServers": {
    "bobby-browser": {
      "url": "http://127.0.0.1:7777/v1/mcp",
      "transport": "streamable-http",
      "headers": { "Authorization": "Bearer ${AUTOMATION_RUNTIME_TOKEN}" }
    }
  }
}
```

Check the connection:

```bash
curl -sS http://127.0.0.1:7777/v1/mcp \
  -H "Authorization: Bearer ${AUTOMATION_RUNTIME_TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}'
```

## Behavior

- Each `POST` carries one JSON-RPC message. Send `initialize` first.
- `GET /v1/mcp` opens a server-sent event stream with the principal's events as `notifications/bobby/event` and `notifications/tools/list_changed` frames. An idle stream sends a keep-alive comment every 15 seconds. A principal without `session:read` gets only control frames.
- MCP state is per principal. Clients sharing a principal share initialization state, so a new `initialize` resets all of them. Issue one principal per client to isolate them.
- Rotating or replacing a bearer resets that principal's state. Send `initialize` again.

See [Authentication](../guides/auth.md) to issue scoped tokens and [OpenShell host](../guides/openshell.md) for sandboxes.
