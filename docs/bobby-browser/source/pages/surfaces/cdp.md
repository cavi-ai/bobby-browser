---
documentedVersion: {{PRODUCT_VERSION}}
---

# Authenticated CDP

Connect existing Playwright or Puppeteer scripts to bobby over the Chrome DevTools Protocol with bearer authentication. CDP here is a narrow compatibility surface for connecting, navigating, taking screenshots and simple form actions. For intents, evidence, recovery and arbitrary reads, use [MCP](mcp-tools.md), [HTTP](http-api.md) or an SDK.

## Start it

CDP runs on its own listener, not on the `/v1` HTTP router.

```bash
export AUTOMATION_RUNTIME_BROWSER_SELECTION='{"preference":{"mode":"managedChromium"}}'
bobby cdp --cdp-port 9333
export AUTOMATION_RUNTIME_TOKEN="$(bobby token)"
```

`bobby cdp` runs the runtime with CDP bound to `[cdp].host` and `[cdp].port` (default `127.0.0.1:9222`). `bobby serve` can bind it too when `[cdp].enabled = true`.

| `[cdp]` key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Bind CDP when running `bobby serve` |
| `host` | `127.0.0.1` | Bind address |
| `port` | `9222` | Listen port. `bobby doctor` warns on a conflict (`cdp-port`) |
| `auto_session` | `true` | Open a session and blank page for a client that holds `session:write` and `page:write` and has none |

CDP cannot create a session. With `auto_session = false`, open a session and page first over HTTP, MCP or an SDK.

## Connect

Send exactly one `Authorization: Bearer <token>` header on every discovery request and WebSocket upgrade. Tokens in URLs are rejected.

Playwright:

```ts
import { chromium } from "playwright-core";

const browser = await chromium.connectOverCDP("http://127.0.0.1:9333", {
  headers: { Authorization: `Bearer ${process.env.AUTOMATION_RUNTIME_TOKEN!}` },
});
const page = browser.contexts()[0].pages()[0];
await page.goto("https://example.com");
await page.screenshot({ path: "example.png" });
```

Puppeteer:

```ts
import puppeteer from "puppeteer-core";

const headers = { Authorization: `Bearer ${process.env.AUTOMATION_RUNTIME_TOKEN!}` };
const version = await (await fetch("http://127.0.0.1:9333/json/version", { headers })).json();
const browser = await puppeteer.connect({
  browserWSEndpoint: version.webSocketDebuggerUrl,
  headers,
});
const page = (await browser.pages())[0] ?? (await browser.newPage());
```

Supported clients are `playwright-core` 1.61 to 1.63 and `puppeteer-core` 25. A newer release fails closed on every page it opens.

Discovery is `GET /json/version` and `GET /json/list`. The only WebSocket is `/devtools/browser/<id>`; every target is addressed over it. `/json/list` reports `about:blank` for a page the gateway has not navigated.

## What works

| Operation | Playwright | Puppeteer |
|---|---|---|
| Connect, navigate, screenshot | yes | yes |
| Use the page opened at connect | `contexts()[0].pages()[0]` | not listed; call `newPage()` |
| Open a page from the client | no | `browser.newPage()` |
| Viewport emulation | yes, with `deviceScaleFactor` 1 | yes, same limit |
| Fill a labeled field, click a named button or link, set input files | yes, by label or role and name | only the operations covered by the support list |
| CSS or XPath selectors, `waitForSelector`, `content`, `$eval` | no | no |
| `evaluate` | no, use `evaluate_javascript` | no |
| PDF, cookies, request interception | no | no |

Puppeteer refuses `deviceScaleFactor` other than 1, non-portrait `screenOrientation` and `hasTouch`. Pass `defaultViewport: null` to skip viewport emulation.

`Runtime.evaluate` accepts only the clients' own injected scripts. Any other expression is refused with `unrecognized bounded runtime bootstrap`. Run page JavaScript through `evaluate_javascript`, which needs `javascript:evaluate` and `executionPolicy.javascriptEvaluation`.

The full method allowlist and unsupported domains are in [`docs/cdp-support.json`](https://github.com/cavi-ai/bobby-browser/blob/main/docs/cdp-support.json).

## Next

- [HTTP API](http-api.md)
- [Capabilities](../concepts/capabilities.md)
- [Security model](../security/model.md)
