---
documentedVersion: {{PRODUCT_VERSION}}
---

# MCP stdio

Local agent hosts run `bobby mcp-stdio`. It loads the bootstrap credential itself, connects to the shared local runtime (starting it if needed), and speaks MCP over stdin and stdout. Tools are listed in [MCP tools](mcp-tools.md).

## Configure a host

`bobby install` writes the entry for you:

```bash
bobby install --host claude --yes
```

The entry carries no credential:

```json
{
  "mcpServers": {
    "bobby-browser": {
      "command": "/usr/local/bin/bobby",
      "args": ["mcp-stdio"]
    }
  }
}
```

`bobby init --emit <claude|zed|vscode|json>` prints this fragment for hosts you configure by hand. `--emit openshell` prints the HTTP client form.

## Shared runtime

All agents in a scope share one runtime owner on a loopback port. Each connection keeps its own MCP state and starting toolset. Browser profiles, storage, context and jobs belong to the owner.

- Disconnecting an agent closes the sessions that connection opened. Sessions of other connections stay.
- `bobby runtime start`, `status`, `stop`, `restart` and `list` manage the owner. `stop` and `restart` ask before disconnecting attached agents. Without a terminal, add `--disconnect-agents`.
- `bobby runtime status` reports a pending change, such as a new binary. `bobby runtime restart` applies it.
- Add `--team <name>` and `--project <name>` to any command to use an organized local scope. The scope is shared per OS user.

## How many agents

Each attached agent uses one of `interface.max_connections` slots (default 64). When all are in use, a new agent is refused with `resourceExhausted` and a retry hint. Raise the setting to attach more.

## Direct gateway

Hosts that cannot run `bobby` can launch the `mcp-gateway` binary. It needs four environment variables, which `bobby init` writes to `bootstrap.env`:

| Variable | Purpose |
|---|---|
| `AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN` | Bearer token, 32 to 505 printable ASCII bytes |
| `AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL` | Principal UUID |
| `AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES` | Comma-separated capabilities |
| `AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT` | RFC 3339 expiry |

Missing or invalid values stop startup. `bobby init --preset` selects the capability floor: `agent` (default, no `authority:admin`), `unrestricted`, `claude`, `codex` or `openshell`. See [Capabilities](../concepts/capabilities.md).

## Limits

- Frames up to 1 MiB, tool input up to 256 KiB, event reads up to 256 records.
- Send `initialize` before any tool call.
- Stdout carries only JSON-RPC. Logs go to stderr.

`bobby doctor` performs a live `initialize` and `tools/list` and reports the catalog size against its 128 KiB budget.
