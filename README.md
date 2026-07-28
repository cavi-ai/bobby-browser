# bobby-browser

A browser automation runtime with authenticated, capability-scoped control
surfaces: Rust and TypeScript SDKs, MCP (stdio and streamable HTTP), and
Playwright/Puppeteer over authenticated CDP. All adapters share the same
capability, idempotency, evidence, checkpoint, and event contracts.
Authentication fails closed; credentials are never accepted in URLs or query
strings.

## Run the runtime

```bash
cargo build -p cli --release
./target/release/bobby init
./target/release/bobby serve
```

Then open `http://127.0.0.1:7777/healthz`.

## Use from TypeScript

```bash
npm install @bobby-browser/sdk
```

```ts
import { BrowserRuntimeClient } from "@bobby-browser/sdk";

const client = new BrowserRuntimeClient({
  baseUrl: "http://127.0.0.1:7777",
  bearerToken: process.env.AUTOMATION_RUNTIME_TOKEN!,
});
```

> **Alpha.** The interfaces and contracts described here are stable enough to
> build against, but may still change before 1.0. See
> [SECURITY.md](SECURITY.md) for the security model and reporting.

**Documentation:** [Online docs](https://cavi-ai.xyz/docs/bobby-browser) ·
[Overview](docs/bobby-browser/source/pages/introduction/overview.md) ·
[Quick start](docs/bobby-browser/source/pages/introduction/quickstart.md) ·
[Docs consumer contract](docs/bobby-browser/CONSUMER.md) ·
[Contributing](CONTRIBUTING.md)

## Learn more

These pages are also served at
[cavi-ai.xyz/docs/bobby-browser](https://cavi-ai.xyz/docs/bobby-browser).

- [Authentication](docs/bobby-browser/source/pages/guides/auth.md)
- [Configuration](docs/bobby-browser/source/pages/guides/configuration.md)
- [Run the server](docs/bobby-browser/source/pages/guides/run.md)
- [JavaScript evaluation](docs/bobby-browser/source/pages/guides/javascript-eval.md)
- [Intents](docs/bobby-browser/source/pages/guides/intents.md)
- [Bobby skills](docs/bobby-browser/source/pages/guides/skills.md)
- [Browser gauntlet](docs/bobby-browser/source/pages/guides/gauntlet.md)
- [Events and recovery](docs/bobby-browser/source/pages/guides/events-recovery.md)
- [MCP over HTTP](docs/bobby-browser/source/pages/surfaces/mcp-http.md) ·
  [MCP over stdio](docs/bobby-browser/source/pages/surfaces/mcp-stdio.md) ·
  [CDP](docs/bobby-browser/source/pages/surfaces/cdp.md)
- [Capabilities](docs/bobby-browser/source/pages/concepts/capabilities.md) ·
  [Evidence and checkpoints](docs/bobby-browser/source/pages/concepts/evidence-checkpoints.md) ·
  [Multi-principal](docs/bobby-browser/source/pages/concepts/multi-principal.md)

The CDP allowlist is published in
[`docs/cdp-support.json`](docs/cdp-support.json). The same pages are built into
an immutable versioned artifact under
[`docs/bobby-browser/v0.2.0`](docs/bobby-browser/v0.2.0) for documentation hosts.
