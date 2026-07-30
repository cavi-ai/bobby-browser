---
documentedVersion: 0.2.1
---

# Authenticated CDP

The CDP gateway (`cdp-gateway`) exposes Chromium DevTools discovery and
WebSockets with bearer auth. It is a separate control surface from
`bobby serve` HTTP `/v1/*` (conformance boots a gateway example; production
embedding wires the gateway alongside the runtime).

## Discovery and sockets

- `GET /json/version`, `GET /json/list` — discovery
- WebSockets: `/devtools/browser/:id`, `/devtools/page/:id`

Every discovery request and WebSocket upgrade must include exactly one
`Authorization: Bearer <token>` header. Credentials are never accepted in URLs
or query strings.

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

const browser = await chromium.connectOverCDP(endpoint, {
  headers: { Authorization: `Bearer ${process.env.AUTOMATION_RUNTIME_TOKEN!}` },
});
```

```ts
import puppeteer from "puppeteer-core";

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
intents.
