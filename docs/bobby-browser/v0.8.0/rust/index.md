---
documentedVersion: 0.8.0
---

# Rust crate book

This track documents Rust crates for HTTP clients and in-process embedding.
The sole crates.io **library** SDK is `bobby-browser-client` (client + wire types).
The CLI package `bobby-browser` is separate.

## Stability tiers

| Tier | Meaning |
|---|---|
| **Supported** | Documented consumer APIs (`bobby-browser-client`, CLI) |
| **Embed** | Supported for in-process use; alpha may still break |
| **Internal** | Workspace-only; do not treat as a public API |

Publishing is phased. Prefer building from this repo until `cargo add` /
`cargo install` succeed for the version you want.

## Contents

| Page | Tier | Role |
|---|---|---|
| [bobby-browser-client](bobby-browser-client.md) | Supported | crates.io SDK: HTTP `/v1` + wire types |
| [Wire types](types.md) | Supported | Same types, via `bobby-browser-client` |
| [interface-core](interface-core.md) | Embed | Authority, events, authorization |
| [sdk-core](sdk-core.md) | Embed | Runtime service behind the broker |
| [broker](broker.md) | Embed | HTTP `/v1` + MCP HTTP |
| [Engines and stores](engines.md) | Internal | worker-pool, page-runtime, … |

CLI package: `bobby-browser` → binary `bobby` (see [CLI reference](../guides/cli.md)).
