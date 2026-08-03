---
documentedVersion: {{PRODUCT_VERSION}}
---

# MCP stdio

`mcp-gateway` is a single-process MCP server over stdio. It enrolls a startup
bootstrap credential from environment variables, then speaks MCP protocol
version `2025-11-25` on stdin/stdout. Stdout is reserved for newline-delimited
JSON-RPC; diagnostics go to stderr.

## Build

```bash
cargo build -p mcp-gateway --release
# binary: ./target/release/mcp-gateway
```

## Startup credential

At process start the gateway requires all four bootstrap variables (same
contract as `bobby serve` / `bobby init`):

| Variable | Purpose |
|---|---|
| `AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN` | High-entropy plaintext bearer (32–505 printable ASCII bytes) |
| `AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL` | Principal UUID |
| `AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES` | Comma-separated capability wire strings |
| `AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT` | RFC3339 expiry |

Generate them with `bobby init` (writes `…/bobby-browser/bootstrap.env`), then
either export the file into the environment or point your MCP client `env` at
those keys. Missing or invalid startup input fails closed.

There is no single `AUTOMATION_RUNTIME_TOKEN` env var for stdio startup. That
name is only a conventional alias for the **client** bearer when talking to the
HTTP runtime / TypeScript SDK.

## Client config example

`bobby init --emit <claude|zed|vscode|json>` prints the right fragment for
your host (no credential material — `${VAR}` placeholders only):

```json
{
  "mcpServers": {
    "bobby-browser": {
      "command": "mcp-gateway",
      "env": {
        "AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN": "${AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN}",
        "AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL": "${AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL}",
        "AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES": "${AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES}",
        "AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT": "${AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT}"
      }
    }
  }
}
```

`bobby doctor` runs a live handshake against the gateway (`initialize` +
`tools/list`) and reports the tool count and byte size against the 128 KiB
catalog budget, so a dead or oversized surface is caught before the agent
sees it.

## Limits and lifecycle

- Frames limited to 1 MiB; tool input to 256 KiB; event reads to 256 records
- Call `initialize` before tools
- Cancellation, EOF, expiry, and revocation close or reject work without leaking credentials

Tool catalog: [MCP tools](mcp-tools.md). Multi-tenant HTTP alternative: [MCP over HTTP](mcp-http.md).
