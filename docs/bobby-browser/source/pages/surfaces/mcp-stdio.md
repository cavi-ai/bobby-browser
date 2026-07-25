---
documentedVersion: 0.2.0
---

# MCP stdio

Launch `mcp-gateway` with the credential delivered through its protected startup channel. Configure an MCP client to run the binary directly; stdout is reserved exclusively for newline-delimited JSON-RPC and diagnostics go to stderr.

The supported protocol version is `2025-11-25`; frames are limited to 1 MiB, tool input to 256 KiB, and event reads to 256 records. Initialize before calling tools. Cancellation, EOF, expiry, and revocation close or reject work without leaking credentials.

```json
{
  "mcpServers": {
    "bobby-browser": {
      "command": "mcp-gateway",
      "env": { "AUTOMATION_RUNTIME_TOKEN": "${AUTOMATION_RUNTIME_TOKEN}" }
    }
  }
}
```
