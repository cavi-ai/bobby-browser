---
documentedVersion: 0.5.0
---

# MCP over streamable HTTP

The served runtime (`bobby serve`) exposes the same MCP tool surface over
streamable HTTP at `POST /v1/mcp` with bearer-only auth. Each tenant needs a
URL and its scoped token.

One JSON-RPC message per `POST`. `GET /v1/mcp` opens the streamable-HTTP
SSE channel (keep-alive comments only today; no server-initiated messages yet).
Clients that expect a GET channel for streamable HTTP should open it with the
same bearer and interface headers as `POST`. Server state is isolated per
principal. A rotated or replaced bearer resets that principal's MCP
lifecycle — clients must `initialize` again.

## Required HTTP headers

MCP over HTTP uses the same broker context headers as `/v1/*`:

| Header | Example |
|---|---|
| `Authorization` | `Bearer ${AUTOMATION_RUNTIME_TOKEN}` |
| `x-interface-version` | `2026-07-23` |
| `x-correlation-id` | UUID |
| `x-deadline` | RFC3339 UTC time in the future |
| `Content-Type` | `application/json` |

## Client config example

```json
{
  "mcpServers": {
    "bobby-browser": {
      "url": "http://127.0.0.1:7777/v1/mcp",
      "transport": "streamable-http",
      "headers": {
        "Authorization": "Bearer ${AUTOMATION_RUNTIME_TOKEN}",
        "x-interface-version": "2026-07-23",
        "x-correlation-id": "00000000-0000-4000-8000-000000000001",
        "x-deadline": "2099-01-01T00:00:00Z"
      }
    }
  }
}
```

Prefer an MCP client that can refresh `x-correlation-id` and `x-deadline` per
request. For a single-process local agent, [MCP stdio](mcp-stdio.md) may be
simpler.

## curl smoke (`initialize`)

```bash
CORRELATION=$(uuidgen | tr '[:upper:]' '[:lower:]')
DEADLINE=$(date -u -v+60S +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d '+60 seconds' +%Y-%m-%dT%H:%M:%SZ)
curl -sS http://127.0.0.1:7777/v1/mcp \
  -H "Authorization: Bearer ${AUTOMATION_RUNTIME_TOKEN}" \
  -H "x-interface-version: 2026-07-23" \
  -H "x-correlation-id: ${CORRELATION}" \
  -H "x-deadline: ${DEADLINE}" \
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
- End-to-end loop: [First browser session](../introduction/first-session.md)
- Troubleshooting: [Troubleshooting](../guides/troubleshooting.md)
