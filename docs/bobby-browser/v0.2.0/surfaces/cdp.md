---
documentedVersion: 0.2.0
---

# Authenticated CDP

Discovery is available at `/json/version` and `/json/list`; WebSockets use `/devtools/browser/:id` and `/devtools/page/:id`. Every discovery request and WebSocket upgrade must include `Authorization: Bearer <token>`.

Connect with Playwright `1.61.1` via `chromium.connectOverCDP(endpoint, { headers })`, or Puppeteer `25.3.0` via `puppeteer.connect({ browserWSEndpoint, headers })`.

The compiled allowlist, client coverage, and explicitly unsupported domains are published in [`docs/cdp-support.json`](https://github.com/cavi-ai/bobby-browser/blob/main/docs/cdp-support.json). Raw CDP forwarding is intentionally unsupported.

Playwright, Puppeteer, and raw CDP remain **primitives-only** — there is no parallel intent API on those adapters.
