---
documentedVersion: 0.12.0
---

# broker

**Tier: Embed**

Authenticated HTTP `/v1/*`, MCP over HTTP (`POST /v1/mcp`), startup credential
loading, and principal issuance/revocation. Usually entered via `bobby serve`
rather than as a direct library dependency.

Library use is for embedding a custom binary that still speaks the same wire
contracts. Prefer the CLI for operators and `bobby-browser-client` for remote
Rust apps.

## Next

- [HTTP API](../surfaces/http-api.md)
- [CLI reference](../guides/cli.md)
- [MCP over HTTP](../surfaces/mcp-http.md)
