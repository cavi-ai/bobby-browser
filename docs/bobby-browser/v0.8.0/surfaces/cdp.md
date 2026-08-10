---
documentedVersion: 0.8.0
---

# Authenticated CDP

The CDP gateway (`cdp-gateway`) exposes Chromium DevTools discovery and
WebSockets with bearer auth. It is a **separate** control surface from
`bobby serve` HTTP `/v1/*` — discovery and sockets bind on a dedicated TCP
listener, not the HTTP router.

`bobby cdp` starts the runtime like `bobby serve` and binds authenticated CDP
on `[cdp].host`:`[cdp].port` (default `127.0.0.1:9222`). Plain `bobby serve`
leaves CDP off unless `[cdp].enabled = true`.

```toml
[cdp]
enabled = true   # default false
host = "127.0.0.1"
port = 9222
```

Both commands share the same broker, sessions, and bearer credentials. Override
the CDP port at the CLI with `bobby cdp --cdp-port 9333`.

## Discovery and sockets

- `GET /json/version`, `GET /json/list` — discovery
- WebSockets: `/devtools/browser/:id`, `/devtools/page/:id`

Every discovery request and WebSocket upgrade must include exactly one
`Authorization: Bearer <token>` header. Credentials are never accepted in URLs
or query strings. Long-lived sockets re-check capability and expiry.

## Connect snippets

Pinned clients in this repo (see `packages/interface-conformance/package.json`
and `pnpm-lock.yaml`):

- `playwright-core` **1.62.0**
- `puppeteer-core` **25.4.0**

(`docs/cdp-support.json` still labels some parameter schema revisions with
historical `playwright-1.61.1-*` strings; use the lockfile versions for client
installs.)

```ts
import { chromium } from "playwright-core";

const endpoint = "http://127.0.0.1:9222";

const browser = await chromium.connectOverCDP(endpoint, {
  headers: { Authorization: `Bearer ${process.env.AUTOMATION_RUNTIME_TOKEN!}` },
});
```

```ts
import puppeteer from "puppeteer-core";

// Fetch webSocketDebuggerUrl from GET http://127.0.0.1:9222/json/version
const browser = await puppeteer.connect({
  browserWSEndpoint: wsEndpoint,
  headers: { Authorization: `Bearer ${process.env.AUTOMATION_RUNTIME_TOKEN!}` },
});
```

## Allowlist and limits

The compiled allowlist, client coverage, and explicitly unsupported domains are
published in
[`docs/cdp-support.json`](https://github.com/cavi-ai/bobby-browser/blob/main/docs/cdp-support.json).
Raw CDP forwarding of arbitrary methods is intentionally unsupported.

Playwright, Puppeteer, and raw CDP remain **primitives-only** — there is no
parallel intent API on those adapters. Use HTTP / MCP / TypeScript SDK for
intents (`activatePage` included as a primitive on those surfaces).

## When to use CDP vs HTTP

| Need | Prefer |
|---|---|
| Navigate / click / intents / evidence / recovery | HTTP, MCP, or TS/Rust SDK |
| Existing Playwright/Puppeteer scripts against DevTools | Authenticated CDP |
| Arbitrary CDP domains outside the allowlist | Not supported |

## Next

- [HTTP API](http-api.md)
- [Capabilities](../concepts/capabilities.md)
- [Security model](../security/model.md)
