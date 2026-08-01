# bobby-browser

A browser automation runtime with authenticated, capability-scoped control
surfaces: Rust and TypeScript SDKs, MCP (stdio and streamable HTTP), and
Playwright/Puppeteer over authenticated CDP. All adapters share the same
capability, idempotency, evidence, checkpoint, and event contracts.
Authentication fails closed; credentials are never accepted in URLs or query
strings.

## Run the runtime

```bash
cargo build -p bobby-browser --release
./target/release/bobby --help
./target/release/bobby init
./target/release/bobby doctor
./target/release/bobby serve --config ./config.toml
```

Then open `http://127.0.0.1:7777/healthz`. CLI details:
[docs CLI reference](docs/bobby-browser/source/pages/guides/cli.md).

## Use from TypeScript

Package: `@cavi-ai/bobby-browser` (publish via `sdk-v*` tag / Publish npm workflow).

```bash
npm install @cavi-ai/bobby-browser
```

```ts
import { BrowserRuntimeClient } from "@cavi-ai/bobby-browser";

const client = new BrowserRuntimeClient({
  baseUrl: "http://127.0.0.1:7777",
  bearerToken: process.env.AUTOMATION_RUNTIME_TOKEN!,
});
```

Until the package is on the public registry, build from this repo:

```bash
pnpm install
pnpm --filter @cavi-ai/bobby-browser test
```

## Use from Rust (HTTP)

Package: `bobby-browser-client` (crates.io after publish). Crate book:
[docs/bobby-browser/source/pages/rust/index.md](docs/bobby-browser/source/pages/rust/index.md).

```bash
cargo add bobby-browser-client
# from this repo:
cargo test -p bobby-browser-client
```

```rust,no_run
use bobby_browser_client::BrowserRuntimeClient;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let client = BrowserRuntimeClient::new(
    "http://127.0.0.1:7777",
    std::env::var("AUTOMATION_RUNTIME_TOKEN")?,
)?;
let _info = client.runtime_info(None).await?;
# Ok(()) }
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
[`docs/bobby-browser/v0.3.1`](docs/bobby-browser/v0.3.1) for documentation hosts.
