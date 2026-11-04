# bobby-browser

A browser automation runtime for AI agents and applications. It drives Firefox or Chromium and exposes the browser through MCP tools, an HTTP API, ACP, authenticated CDP, and SDKs for TypeScript, Python and Rust.

Every call is authenticated, limited by capabilities, journaled with evidence, and recoverable after a crash. Credentials are never accepted in URLs.

> **Alpha.** Interfaces are stable enough to build against but can change before 1.0. See [SECURITY.md](SECURITY.md).

## Fill the form. Check the result.

Use Bobby to fill a form you control, inspect the returned field evidence,
then submit with an expected result. Completing the form does not submit it.

[Watch the 58-second screenshot walkthrough](assets/form-walkthrough.mp4)
— an edited sequence from a **synthetic local form**, not a continuous recording.

| Task | Action | Observed result |
| :-- | :-- | :-- |
| Reveal team fields and save a sample request | Reveal the fields, inspect required-field failures, correct the remaining values, then submit separately with a saved-receipt check | The corrected email returned exact-value and validity evidence. The separate submit produced `DEMO-001`, with matching data saved by the local demo server. |

One synthetic form does not establish reliability on other websites. Field
validity does not prove server acceptance.

[Install below](#install), then [try a controlled form](#try-a-controlled-form).

## Install

Pick one.

```bash
# Homebrew
brew tap cavi-ai/tap
brew install cavi-ai/tap/bobby-browser

# Install script (Linux and macOS)
curl -fsSL https://raw.githubusercontent.com/cavi-ai/bobby-browser/main/scripts/install.sh | bash

# From source (needs the Rust toolchain, Node and pnpm)
git clone https://github.com/cavi-ai/bobby-browser.git
cd bobby-browser
make install
```

Homebrew and the install script put `bobby`, `mcp-gateway` and `acp-gateway` on your `PATH`. `make install` builds those three binaries in release mode and builds the Firefox companion extension. It then runs `bobby install`, which puts the binaries on `PATH`, installs the companion, wires your agent host and creates a credential if none exists. It shows the checklist in a terminal and runs unattended without one. `make cli` installs only the binaries. Windows and release archives are covered in [Installation](docs/bobby-browser/source/pages/introduction/installation.md).

## First session

Wire up your agent host and check the setup:

```bash
bobby install
bobby doctor
```

`bobby install` creates a credential and writes the MCP entry for Claude Code, VS Code, Zed, an ACP host or OpenShell. Add `--skill` to install the agent skill. For Firefox, run `bobby install --companion`, then `bobby firefox-start` and click **Pair** in the Bobby Companion toolbar popup. After `make install`, the companion is already installed.

Restart your agent host. The agent then calls `workflow_start` with `{"profile": "default", "url": "https://example.com"}`, `workflow_observe` with the returned handle, and `click`, `type_text` or an `intent_*` tool to act. See the [Quickstart](docs/bobby-browser/source/pages/introduction/quickstart.md).

### Try a controlled form

After setup, open a test form you own and ask your agent:

> Inspect this form with Bobby. Fill it with sample data, including any fields
> revealed by earlier choices. Show the field evidence before submitting. Submit
> once only when the values are correct, using the confirmation this form actually
> returns, then inspect the saved result.

The agent reuses one `workflowHandle`, reads `form_snapshot`, and calls
`intent_complete_form` with the observed fields. If a field fails, inspect the
returned evidence and correct only the remaining fields with `intent_fill`.
`intent_submit_and_verify` is a separate call with the expected confirmation.
If the submit outcome is uncertain, inspect the destination before another
submit. Close the task session with `session_close` when finished.

See [form arguments and expected-state examples](docs/bobby-browser/source/pages/guides/intents.md#fill-a-whole-form).
If the workflow stops unexpectedly, [report a reproducible issue](https://github.com/cavi-ai/bobby-browser/issues)
with the tool, error and expected result, omitting tokens and private form values.
Documentation improvements are welcome through [CONTRIBUTING.md](CONTRIBUTING.md).

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

The [Cavi API client example](examples/cavi-api-client/README.md) reads page text
with Bobby and streams a summary through a selected model provider.

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

See [CONTRIBUTING.md](CONTRIBUTING.md) and [TESTING.md](TESTING.md). The CDP method allowlist is [`docs/cdp-support.json`](docs/cdp-support.json). The docs are built from `docs/bobby-browser/source` with `pnpm docs:build` into the release asset `bobby-browser-docs-v0.22.1.tar.gz`; the [consumer contract](docs/bobby-browser/CONSUMER.md) describes it.
