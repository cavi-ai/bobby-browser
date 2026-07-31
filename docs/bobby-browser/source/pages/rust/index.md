---
documentedVersion: {{PRODUCT_VERSION}}
---

# Rust crate book

This track documents Rust crates consumers may depend on from crates.io (or
from this workspace). Product docs for operators stay under Introduction /
Guides / Surfaces; this book is for embedding and HTTP clients in Rust.

## Stability tiers

| Tier | Meaning |
|---|---|
| **Supported** | Documented consumer APIs for remote or shared use |
| **Embed** | Supported for in-process use; alpha may still break |
| **Internal** | Published so `cargo install bobby-browser` works; do not treat as a public API |

Publishing is phased. Prefer building from this repo until `cargo add` /
`cargo install` succeed for the version you want.

## Contents

| Page | Tier | Role |
|---|---|---|
| [bobby-browser-client](bobby-browser-client.md) | Supported | HTTP client over `/v1` |
| [types](types.md) | Supported | Wire types and interface version |
| [interface-core](interface-core.md) | Embed | Authority, events, authorization |
| [sdk-core](sdk-core.md) | Embed | Runtime service behind the broker |
| [broker](broker.md) | Embed | HTTP `/v1` + MCP HTTP |
| [Engines and stores](engines.md) | Internal | worker-pool, page-runtime, … |

CLI package: `bobby-browser` → binary `bobby` (see [CLI reference](../guides/cli.md)).
