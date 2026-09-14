---
documentedVersion: {{PRODUCT_VERSION}}
---

# Overview

bobby-browser is a browser automation runtime with authenticated,
capability-scoped control surfaces. All adapters share capability, idempotency,
evidence, checkpoint, and event contracts. Authentication fails closed;
credentials are never accepted in URLs or query strings.

> **Alpha.** Interfaces are stable enough to build against, but may still change
> before 1.0. See the [security model](../security/model.md) before exposing any
> deployment. Interface version: **`{{INTERFACE_VERSION}}`**.

## Which surface?

| Goal | Start here |
|---|---|
| Install / run the CLI | [Installation](installation.md) · [CLI reference](../guides/cli.md) |
| First successful navigate | [First browser session](first-session.md) |
| Application code in Node/TS | [TypeScript SDK](../surfaces/typescript-sdk.md) |
| Application code in Rust (HTTP) | [bobby-browser-client](../rust/bobby-browser-client.md) |
| Embed in Rust | [Rust crate book](../rust/index.md) · [Rust SDK](../surfaces/rust-sdk.md) |
| Raw HTTP / curl | [HTTP API](../surfaces/http-api.md) + [Authentication](../guides/auth.md) |
| Agent host (Claude, Cursor, …) | [MCP tools](../surfaces/mcp-tools.md) via [stdio](../surfaces/mcp-stdio.md) or [HTTP](../surfaces/mcp-http.md) |
| NVIDIA OpenShell sandbox | [OpenShell host](../guides/openshell.md) |
| Playwright / Puppeteer | [Authenticated CDP](../surfaces/cdp.md) (primitives only) |

Default browser engine preference is **Firefox** (with Chromium available when
selected). The runtime is **multi-principal**: one instance serves many tenants,
each with a capability-scoped bearer.
