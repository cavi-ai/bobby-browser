---
documentedVersion: 0.12.0
---

# Engines and stores

**Tier: Internal**

These crates exist so the installable CLI and runtime compile. They are **not** a
supported public library API. Depending on them couples you to internal
refactors without a curated changelog.

| Area | Crates |
|---|---|
| Browsers / workers | `worker-pool`, `firefox-companion`, `companion-*` |
| Page / session / intents | `page-runtime`, `session-manager`, `intent-engine` |
| DOM / JS / network | `dom-engine`, `js-engine`, `network-engine` |
| Persistence | `artifact-store`, `checkpoint-store`, `workflow-journal`, `task-scheduler` (in-process queue + optional JSONL journal; broker exposes `POST/GET/DELETE /v1/jobs` with `job:*` capabilities; not a public library API) |
| Gateways | `mcp-gateway`, `cdp-gateway` |
| Other | `skill-runtime`, `observability`, `config` (config is shared but still alpha) |

Public Rust consumers should stick to:

- `bobby-browser-client` + `types` (Supported)
- `interface-core` / `sdk-core` / `broker` only when embedding (Embed)

## Next

- [Rust crate book index](index.md)
- [CLI package `bobby-browser`](../guides/cli.md)
