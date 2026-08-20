---
documentedVersion: 0.11.0
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

9222 belongs to CDP here: `make firefox-start` puts the companion profile's
remote-debugging endpoint on 9224 so the two do not collide. Any other browser
you started yourself on 9222 still owns it first — `bobby doctor` reports that
as `cdp-port` and startup fails naming the address; pick another port with
`--cdp-port`.

## Setup

```bash
# Managed Chromium needs no pairing; the default engine preference
# (Firefox companion) does.
export AUTOMATION_RUNTIME_BROWSER_SELECTION='{"preference":{"mode":"managedChromium"}}'
bobby cdp --cdp-port 9333
```

That is the whole setup. A connecting client that holds `session:write` and
`page:write` and has no session yet gets one opened for it, with a blank page,
so `contexts()[0].pages()[0]` is there on the first read. CDP itself cannot
create a session — `Target.createTarget` reaches an existing one — so without
this a connected client would have nothing to drive.

Set `[cdp].auto_session = false` to turn it off; a client then sees an empty
browser until a session and page are opened over HTTP (`POST /v1/sessions`,
`POST /v1/pages`), MCP (`session_create`, `page_open`), or an SDK.

## Discovery and sockets

- `GET /json/version`, `GET /json/list` — discovery
- WebSocket: `/devtools/browser/:id` — the only socket. Every `/json/list`
  entry carries this same browser-level URL; there is no per-page
  `/devtools/page/:id` endpoint. Targets are addressed by id over the browser
  socket.

`/json/list` reports the URL and title this gateway last verified for a page.
A page it has not navigated reads as `about:blank`, because discovery has no
way to ask the runtime what a page is showing.

Every discovery request and WebSocket upgrade must include exactly one
`Authorization: Bearer <token>` header. Credentials are never accepted in URLs
or query strings. Long-lived sockets re-check capability and expiry.

Get the bearer with `bobby token` (it refuses to write to a redirected stdout
without `--stdout`):

```bash
export AUTOMATION_RUNTIME_TOKEN="$(bobby token)"
```

## Connect snippets

Pinned clients in this repo (see `packages/interface-conformance/package.json`
and `pnpm-lock.yaml`):

- `playwright-core` **1.62.1**
- `puppeteer-core` **25.5.0**

Playwright is pinned by bundle identity, not by version string: the gateway
carries the exact length and SHA-256 of each supported release's injected and
utility scripts. Playwright 1.61 and 1.62 are covered. A newer release needs its
own entry — `docs/cdp-support.json`'s `playwright-1.61.1-*` revision labels name
the schema shape those entries were first cut against, not the only version
accepted.

```ts
import { chromium } from "playwright-core";

const endpoint = "http://127.0.0.1:9222";

const browser = await chromium.connectOverCDP(endpoint, {
  headers: { Authorization: `Bearer ${process.env.AUTOMATION_RUNTIME_TOKEN!}` },
});

// Drive the pages the runtime already opened. Client-side page creation
// (`context.newPage()`, `browser.newPage()`) is not a supported path — see
// below.
const page = browser.contexts()[0].pages()[0];
```

```ts
import puppeteer from "puppeteer-core";

// Fetch webSocketDebuggerUrl from GET http://127.0.0.1:9222/json/version
const browser = await puppeteer.connect({
  browserWSEndpoint: wsEndpoint,
  headers: { Authorization: `Bearer ${process.env.AUTOMATION_RUNTIME_TOKEN!}` },
});

// Puppeteer's own target discovery does not enumerate pre-existing runtime
// pages, so this falls through to newPage(), which opens one.
const page = (await browser.pages())[0] ?? (await browser.newPage());
```

Puppeteer's default viewport is applied through the runtime's own emulation.
What it cannot apply, it refuses rather than ignores: a `deviceScaleFactor`
other than 1, a non-portrait `screenOrientation`, and `hasTouch` each fail with
the reason. Pass `defaultViewport: null` to skip viewport emulation entirely.

## What this surface is

A pinned client shim, not a CDP backend. The gateway recognizes a fixed set of
call shapes and refuses everything else:

- `Runtime.evaluate` accepts only Playwright's own injected-script bootstraps,
  matched by exact length and SHA-256. Any other expression — including `1+1` —
  is refused with `unrecognized bounded runtime bootstrap`.

  This is not a rule against running JavaScript. The runtime evaluates it on
  request through `evaluate_javascript` (MCP / HTTP / SDK), gated by
  `javascript:evaluate` plus `executionPolicy.javascriptEvaluation`. This
  gateway has no path to that evaluator and no remote-object lifetimes to hand
  back, so it refuses rather than pretending. Need JavaScript, use those
  surfaces.
- `Runtime.callFunctionOn` accepts one pinned Puppeteer declaration over a
  closed list of operations.
- There is no `DOM` domain, and no `Target.attachToTarget`.

A client release newer than the pins fails closed on every page it opens. That
is a coverage gap to be filled by adding a pin, not a runtime fault; run with
`RUST_LOG=debug` and the `cdp.runtime.bootstrap_rejected` line reports the
length and digest that arrived.

## Client operations

| Operation | Playwright | Puppeteer |
|---|---|---|
| `connectOverCDP` / `connect` | yes | yes |
| read the page opened at connect | `contexts()[0].pages()[0]` | not enumerated; use `newPage()` |
| open a page client-side | no — `Target.createTarget` is uncovered for Playwright, which fails wiring a target it never sees attached | `browser.newPage()` |
| navigate | yes | yes |
| screenshot | yes | yes |
| viewport emulation | refused when it needs a `deviceScaleFactor` other than 1 | yes, within that same bound |
| fill a labeled field, click a named button or link, set input files | yes, by accessible label or role + name | only the conformance scenario's own operations |
| CSS/XPath selectors, `waitForSelector`, `innerText`, `content`, `$eval` | no | no |
| `evaluate` | no — use `evaluate_javascript` on MCP / HTTP / SDK | no |
| `pdf`, cookies, history, request interception | no | no |

Reads are the sharp edge: an operation resolves through an accessible label or a
role plus name, the way the runtime's own commands address a page. A CSS
selector or an arbitrary expression has nothing to translate to and fails
closed.

Puppeteer is narrower still — its bridge matches a fixed list of
operation/selector pairs rather than translating an arbitrary label. Use
Playwright, or one of the runtime's own surfaces, for anything beyond connect,
navigate, screenshot, and viewport.

For anything the table refuses, use HTTP, MCP, or a TypeScript/Rust SDK: they
drive the runtime's verified command set, including the accessibility snapshot
that replaces DOM scraping.

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
