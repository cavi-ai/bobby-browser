---
documentedVersion: 0.3.0
---

# Engines and stores

**Tier: Internal**

These crates exist so the installable CLI and runtime compile. They are **not** a
supported public library API:

- `worker-pool`, `page-runtime`, `session-manager`, `intent-engine`
- `dom-engine`, `js-engine`, `network-engine`, `skill-runtime`
- `artifact-store`, `checkpoint-store`, `workflow-journal`
- `mcp-gateway`, `cdp-gateway`, `observability`, `companion-*`, `firefox-companion`

Depend on `bobby-browser-client` + `types` (and embed crates only if you know
you are embedding). Engine APIs may change without a curated changelog.

## Next

- [Rust crate book index](index.md)
- [CLI package `bobby-browser`](../guides/cli.md)
