---
documentedVersion: 0.3.0
---

# MCP over streamable HTTP

The served runtime (`bobby serve`) exposes the same MCP tool surface over
streamable HTTP at `POST /v1/mcp` with bearer-only auth. Each tenant needs a
URL and its scoped token.

One JSON-RPC message per `POST`; `GET` is unsupported. Server state is isolated
per principal. A rotated or replaced bearer resets that principal's MCP
lifecycle — clients must `initialize` again.

## Client config example

Use the bootstrap (or issued) bearer in `Authorization`. The TypeScript SDK
convention `AUTOMATION_RUNTIME_TOKEN` is the same plaintext value.

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

MCP over HTTP still requires the broker interface context headers on the
underlying HTTP transport when your client sends them; the browser-runtime
broker path uses `Authorization`, `x-interface-version`, `x-correlation-id`,
and `x-deadline` (see [Authentication](../guides/auth.md)). Prefer an MCP
client that can attach those headers, or use the TypeScript SDK / stdio gateway.

## Lifecycle

Rotating or replacing the bearer resets that principal's MCP session state.
Clients must `initialize` again before tools. Tool catalog and capability
gates: [MCP tools](mcp-tools.md).

## Next

- Tool list and capabilities: [MCP tools](mcp-tools.md)
- Single-process local agent: [MCP stdio](mcp-stdio.md)
- End-to-end loop: [First browser session](../introduction/first-session.md)
- Troubleshooting: [Troubleshooting](../guides/troubleshooting.md)
