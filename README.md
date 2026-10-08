# bobby-browser

A browser automation runtime for AI agents and applications. It drives Firefox or Chromium and exposes the browser through MCP tools, an HTTP API, ACP, authenticated CDP, and SDKs for TypeScript, Python and Rust.

Every call is authenticated, limited by capabilities, journaled with evidence, and recoverable after a crash. Credentials are never accepted in URLs.

> **Alpha.** Interfaces are stable enough to build against but can change before 1.0. See [SECURITY.md](SECURITY.md).

## Install

Pick one.

```bash
# Homebrew
brew tap cavi-ai/tap
brew install cavi-ai/tap/bobby-browser

# Install script (Linux and macOS)
curl -fsSL https://raw.githubusercontent.com/cavi-ai/bobby-browser/main/scripts/install.sh | bash

# From source
cargo build --release -p bobby-browser
./target/release/bobby install --cli
```

Each installs `bobby`, `mcp-gateway` and `acp-gateway`. Windows and release archives are covered in [Installation](docs/bobby-browser/source/pages/introduction/installation.md).

## First session

Wire up your agent host and check the setup:

```bash
bobby install
bobby doctor
```

`bobby install` creates a credential, writes the MCP entry for Claude Code, VS Code, Zed or an ACP host, and installs the agent skill. For Firefox, run `bobby install --companion`, then `bobby firefox-start` and click **Pair** in the Bobby Companion toolbar popup.

Restart your agent host. The agent then calls `workflow_start` with `{"profile": "default", "url": "https://example.com"}`, `workflow_observe` with the returned handle, and `click`, `type_text` or an `intent_*` tool to act. See the [Quickstart](docs/bobby-browser/source/pages/introduction/quickstart.md).

## Use from code

Run `bobby serve`, then export the token with `export AUTOMATION_RUNTIME_TOKEN="$(bobby token)"`.

TypeScript (`npm install @cavi-ai/bobby-browser`):

```ts
import { BrowserRuntimeClient } from "@cavi-ai/bobby-browser";

const client = new BrowserRuntimeClient({
  baseUrl: "http://127.0.0.1:7777",
  bearerToken: process.env.AUTOMATION_RUNTIME_TOKEN!,
});
const info = await client.runtimeInfo();
```

Python (`pip install bobby-browser`):

```python
import os
from bobby_browser import BrowserRuntimeClient

client = BrowserRuntimeClient("http://127.0.0.1:7777", os.environ["AUTOMATION_RUNTIME_TOKEN"])
info = client.runtime_info()
```

Rust (`cargo add bobby-browser-client`):

```rust,no_run
use bobby_browser_client::BrowserRuntimeClient;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let client = BrowserRuntimeClient::new(
    "http://127.0.0.1:7777",
    std::env::var("AUTOMATION_RUNTIME_TOKEN")?,
)?;
let info = client.runtime_info(None).await?;
# let _ = info;
# Ok(()) }
```

## Documentation

Hosted at [cavi-ai.xyz/docs/bobby-browser](https://cavi-ai.xyz/docs/bobby-browser).

- Start: [Overview](docs/bobby-browser/source/pages/introduction/overview.md), [Installation](docs/bobby-browser/source/pages/introduction/installation.md), [Quickstart](docs/bobby-browser/source/pages/introduction/quickstart.md), [First session from code](docs/bobby-browser/source/pages/introduction/first-session.md)
- Guides: [Intent commands](docs/bobby-browser/source/pages/guides/intents.md), [Events and recovery](docs/bobby-browser/source/pages/guides/events-recovery.md), [Firefox companion](docs/bobby-browser/source/pages/guides/firefox-companion.md), [Troubleshooting](docs/bobby-browser/source/pages/guides/troubleshooting.md)
- Reference: [CLI](docs/bobby-browser/source/pages/guides/cli.md), [Configuration](docs/bobby-browser/source/pages/guides/configuration.md), [HTTP API](docs/bobby-browser/source/pages/surfaces/http-api.md), [MCP tools](docs/bobby-browser/source/pages/surfaces/mcp-tools.md), [Rust SDK](docs/bobby-browser/source/pages/rust/index.md), [Python SDK](docs/bobby-browser/source/pages/surfaces/python-sdk.md), [TypeScript SDK](docs/bobby-browser/source/pages/surfaces/typescript-sdk.md)
- Concepts: [Capabilities](docs/bobby-browser/source/pages/concepts/capabilities.md), [Evidence and checkpoints](docs/bobby-browser/source/pages/concepts/evidence-checkpoints.md), [Multi-principal runtime](docs/bobby-browser/source/pages/concepts/multi-principal.md)

Packages: [GitHub releases](https://github.com/cavi-ai/bobby-browser/releases/latest), [npm](https://www.npmjs.com/package/@cavi-ai/bobby-browser), [PyPI](https://pypi.org/project/bobby-browser/), [crates.io](https://crates.io/crates/bobby-browser-client) ([docs.rs](https://docs.rs/bobby-browser-client)), [Homebrew tap](https://github.com/cavi-ai/homebrew-tap).

## Contributing

```bash
make build   # workspace and gateways
make test    # workspace tests
make lint    # clippy and format check
```

See [CONTRIBUTING.md](CONTRIBUTING.md) and [TESTING.md](TESTING.md). The CDP method allowlist is [`docs/cdp-support.json`](docs/cdp-support.json). The docs are built from `docs/bobby-browser/source` with `pnpm docs:build` into the release asset `bobby-browser-docs-v0.19.1.tar.gz`; the [consumer contract](docs/bobby-browser/CONSUMER.md) describes it.
