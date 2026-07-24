---
documentedVersion: 0.2.0
---

# Overview

bobby-browser is a browser automation runtime with authenticated, capability-scoped control surfaces:

- Rust SDK
- TypeScript SDK over HTTP
- MCP over stdio
- MCP over streamable HTTP (`POST /v1/mcp`) — the multi-tenant driver surface
- Playwright over authenticated CDP
- Puppeteer over authenticated CDP

All adapters use the same capability, idempotency, evidence, checkpoint, and event contracts. Authentication and authorization fail closed; credentials are never accepted in URLs or query strings.

The runtime is **multi-principal**: one instance serves many independent tenants, each with its own capability-scoped bearer token, per-principal in-flight quota, and a token store that survives restart.

> **Alpha.** Interfaces are stable enough to build against, but may still change before 1.0. See the [security model](../security/model.md) before exposing any deployment.
