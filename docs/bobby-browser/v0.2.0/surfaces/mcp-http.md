---
documentedVersion: 0.2.0
---

# MCP over streamable HTTP

For multi-tenant use, the served runtime exposes the same MCP tool surface over streamable HTTP at `POST /v1/mcp` with bearer-only auth. Each tenant needs only a URL and its scoped token.

One JSON-RPC message per `POST`; `GET` is unsupported. Server state is isolated per principal, and a rotated token resets that principal's MCP lifecycle (re-`initialize`).

```json
{
  "mcpServers": {
    "bobby-browser": {
      "url": "http://127.0.0.1:7777/v1/mcp",
      "transport": "streamable-http",
      "headers": { "Authorization": "Bearer ${BOBBY_BROWSER_TOKEN}" }
    }
  }
}
```
