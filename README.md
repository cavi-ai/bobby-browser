# bobby-browser

A browser automation runtime for agents, with authenticated, capability-scoped
control surfaces: MCP (stdio and streamable HTTP), ACP over stdio, Rust and
TypeScript SDKs, and Playwright/Puppeteer over authenticated CDP. All adapters
share the same capability, idempotency, evidence, checkpoint, and event
contracts. Authentication fails closed; credentials are never accepted in URLs
or query strings.

## Install

One command builds the runtime, mints a local credential, wires your agent host,
and installs the agent skill:

```bash
make install
```

Firefox companion only (extension + native host):

```bash
make firefox
```

Put `bobby` on your PATH (`~/.cargo/bin` when that is already on PATH, else
`~/.local/bin`):

```bash
make cli
```

It runs an interactive checklist. For CI or scripted setup:

```bash
cargo build --release -p bobby-browser
./target/release/bobby install --host claude --skill --yes
```

`bobby install` merges into an existing Claude Code, Zed, or VS Code MCP config
rather than replacing it, and writes no secrets into host config — the host
points at `bobby mcp-stdio`, which loads the credential itself.

Verify, then run:

```bash
bobby doctor          # config, credential, storage, browsers, MCP handshake
bobby serve           # http://127.0.0.1:7777/healthz
```

[CLI reference](docs/bobby-browser/source/pages/guides/cli.md) ·
[Run the server](docs/bobby-browser/source/pages/guides/run.md)

## Use from TypeScript

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

## Use from Rust

```bash
cargo add bobby-browser-client
```

```rust,no_run
use bobby_browser_client::{BrowserRuntimeClient, CreateSessionRequest};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let client = BrowserRuntimeClient::new(
    "http://127.0.0.1:7777",
    std::env::var("AUTOMATION_RUNTIME_TOKEN")?,
)?;
let _info = client.runtime_info(None).await?;
let _session = client
    .create_session(
        &CreateSessionRequest {
            profile: "default".into(),
            proxy: None,
            execution_policy: Default::default(),
        },
        None,
    )
    .await?;
# Ok(()) }
```

[Crate book](docs/bobby-browser/source/pages/rust/index.md)

## Published artifacts

One version, one `v*` tag, three artifacts:

| Artifact | Name |
|---|---|
| Binary | `bobby` — GitHub release assets |
| npm | [`@cavi-ai/bobby-browser`](https://www.npmjs.com/package/@cavi-ai/bobby-browser) |
| crates.io | `bobby-browser-client` |

Everything else in `crates/` and `packages/` is implementation and is not
published. `scripts/check-version-agreement.py` enforces this in CI.

> **Alpha.** The interfaces and contracts here are stable enough to build
> against, but may still change before 1.0. See [SECURITY.md](SECURITY.md) for
> the security model and reporting.

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

## Contributing

```bash
make build            # workspace + gateways
make test             # workspace tests
make lint             # clippy -D warnings + fmt check
pnpm install && pnpm --filter @cavi-ai/bobby-browser test
```

The CDP allowlist is published in
[`docs/cdp-support.json`](docs/cdp-support.json). The same pages are built into
an immutable versioned artifact under
[`docs/bobby-browser/v0.5.1`](docs/bobby-browser/v0.5.1) for documentation hosts.
