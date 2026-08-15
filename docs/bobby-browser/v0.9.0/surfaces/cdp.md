---
documentedVersion: 0.9.0
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

Port 9222 is also Firefox's default remote-debugging port. When a Firefox
companion profile is running, the CDP listener cannot bind it — `bobby doctor`
reports the occupancy as `cdp-port`, and startup fails naming the address. Pick
another port with `--cdp-port`.

## Setup

CDP **attaches to** runtime sessions; it does not create them. A client that
connects with no session open gets `no runtime session is available` on its
first call. Open a session and a page first, then connect.

```bash
# 1. Runtime with CDP on a free port. Managed Chromium needs no pairing;
#    the default engine preference (Firefox companion) does.
export AUTOMATION_RUNTIME_BROWSER_SELECTION='{"preference":{"mode":"managedChromium"}}'
bobby cdp --cdp-port 9333

# 2. Session + page over HTTP (MCP session_create/page_open and the SDKs do the
#    same thing). Both calls carry the shared headers from the auth guide.
curl -sS -X POST http://127.0.0.1:7777/v1/sessions … -d '{"profile":"default"}'
curl -sS -X POST http://127.0.0.1:7777/v1/pages    … -d '{"session_id":"<id>"}'
```

Confirm before connecting a client: `GET /json/version` returns the
`webSocketDebuggerUrl`, and `GET /json/list` lists one target per open page.
An empty `/json/list` means step 2 has not run.

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

const page = (await browser.pages())[0];
```

## Client-side page creation

Open pages through the runtime (`POST /v1/pages`, MCP `page_open`, SDK), not
through the client. `Target.createTarget` is allowlisted but only reaches an
existing runtime session, and neither pinned client completes its own
page-creation sequence against the gateway:

| Call | Outcome |
|---|---|
| Playwright `context.newPage()` | `Target.createTarget` is uncovered for Playwright; the client fails while wiring the target it never sees attached |
| Puppeteer `browser.newPage()` | target is created, then `Page.addScriptToEvaluateOnNewDocument` is rejected: only pinned bounded client initialization signatures are supported |

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
