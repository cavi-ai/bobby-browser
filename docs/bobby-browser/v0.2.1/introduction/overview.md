---
documentedVersion: 0.2.1
---

# Overview

bobby-browser is a browser automation runtime with authenticated,
capability-scoped control surfaces. All adapters share capability, idempotency,
evidence, checkpoint, and event contracts. Authentication fails closed;
credentials are never accepted in URLs or query strings.

> **Alpha.** Interfaces are stable enough to build against, but may still change
> before 1.0. See the [security model](../security/model.md) before exposing any
> deployment. Interface version: **`2026-07-23`**.

## Which surface?

| Goal | Start here |
|---|---|
| First successful navigate | [First browser session](first-session.md) |
| Application code in Node/TS | [TypeScript SDK](../surfaces/typescript-sdk.md) |
| Raw HTTP / curl / non-TS | [HTTP API](../surfaces/http-api.md) + [Authentication](../guides/auth.md) |
| Agent host (Claude, Cursor, …) | [MCP tools](../surfaces/mcp-tools.md) via [stdio](../surfaces/mcp-stdio.md) or [HTTP](../surfaces/mcp-http.md) |
| Playwright / Puppeteer | [Authenticated CDP](../surfaces/cdp.md) (primitives only) |
| Embed in Rust | [Rust SDK](../surfaces/rust-sdk.md) |

Surfaces:

- Rust SDK / in-process authority
- TypeScript SDK over HTTP
- MCP over stdio
- MCP over streamable HTTP (`POST /v1/mcp`)
- Playwright / Puppeteer over authenticated CDP

The runtime is **multi-principal**: one instance serves many tenants, each with
a capability-scoped bearer, per-principal in-flight quota, and durable token
records.
