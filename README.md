# bobby-browser

A browser automation runtime for agents, with authenticated, capability-scoped
control surfaces: MCP (stdio and streamable HTTP), ACP over stdio, Rust and
TypeScript SDKs, and Playwright/Puppeteer over authenticated CDP. All adapters
share the same capability, idempotency, evidence, checkpoint, and event
contracts. Authentication fails closed; credentials are never accepted in URLs
or query strings.

## Built for agents

An agent pays for every token it reads and every round trip it makes. The
runtime is shaped around both.

**A catalog you can afford.** `tools/list` opens on a phase, not the whole
surface. The default `explore` phase is 63 KiB and already covers the
standard loop — observe, navigate, click, type, upload, download — so there
is no `toolset_select` before the first action; `full` is 124 KiB.
`toolset_select` widens at any time, and hidden tools stay callable —
phases change what is advertised, never what is permitted. Capability gates
remain the only enforcement boundary. Set the opening phase with
`BOBBY_MCP_TOOLSET` or `[mcp] startup_toolset`.

**Say what you want, not how to click it.** Intents take a purpose — "the
submit button", "the email field" — and resolve it against the accessibility
tree, returning the candidates they considered and why they chose one. Pass an
`a11y_snapshot` node's `target` straight through when you already have it.

**Fewer round trips per action.** A Boundary command needs a pre-action
checkpoint; `autoCheckpoint` mints it inside the same call instead of the three
it used to take. Verified waits report what they observed, so confirming a
submit does not cost a second snapshot.

**Failures tell you what to do next.** Every error carries a machine-readable
repair hint alongside the code, so an agent can act without first reading
`bobby://failure-taxonomy`. A `needsReconciliation` outcome always says the same
thing: do not retry, reconcile first.

**Memory across sessions.** The runtime remembers each site's form structure —
roles, names, ordinals — never typed values or credentials. `context_ask` and
`context_neighbors` answer from it, so a cold session can locate a control
before its first snapshot. `bobby context list` and `bobby context forget
<site>` manage it; a release gate scans the store to prove no values land there.

**It survives losing its place.** `recovery_status` takes a `workflowId`, or a
`sessionId` when a compaction lost it, and lists that session's recoverable
workflows newest-first.

**Work that outlives a call.** `job_submit` / `job_status` / `job_cancel` mirror
HTTP `/v1/jobs`. Built-in handlers: `echo`, `sleep`, `http_probe`, `http_wait`,
and `http_fetch`.

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

### Homebrew (macOS / Linux)

```bash
brew tap cavi-ai/tap
brew install cavi-ai/tap/bobby-browser
```

Installs `bobby`, `mcp-gateway`, and `acp-gateway`. The tap repository is
[`cavi-ai/homebrew-tap`](https://github.com/cavi-ai/homebrew-tap); brew strips
the `homebrew-` prefix and addresses it as `cavi-ai/tap`, so the formula is
reached as `cavi-ai/tap/bobby-browser` rather than repeating the project name.

The formula is named for the project, not the binary, so it matches the crate
and the npm package and stays viable for a homebrew-core submission, where
`bobby` alone would be too generic.

Homebrew rejects a formula outside a tap, so there is no
`brew install --formula ./Formula/...` path; use the tap, or take the binaries
straight from the [release assets](https://github.com/cavi-ai/bobby-browser/releases/latest).

Release archives are three binaries on purpose: the CLI plus the two stdio
gateways agents spawn. The default MCP Explore phase already includes the
base controls; widen with `toolset_select` only for intents, jobs, or escape
hatches.
Use `workflow_start` and `workflow_observe` for lifecycle-safe setup and
retained-first compact context in every MCP phase.

`bobby install` merges into an existing Claude Code, Zed, or VS Code MCP config
rather than replacing it, and writes no secrets into host config — the host
points at `bobby mcp-stdio`, which loads the credential itself.

Verify, then (companion path) start Firefox and Pair once:

```bash
bobby doctor          # config, credential, storage, browsers, MCP handshake
bobby doctor --fix    # repair safe Bobby-owned state, then re-check
make firefox-start    # Bobby profile + --remote-debugging-port=9222; then Pair
```

Doctor uses green/yellow/red status labels in a terminal and stable plain text
when piped or when `NO_COLOR` is set. Add `--download-model` to `doctor --fix`
only when you explicitly want Bobby to fetch the already-selected MLX model.
To configure and load-check a selected MLX model directly, run
`bobby vision connect --yes --provider mlx --model <id> --activate`; add
`--download-model` only when the CLI may fetch a missing cache.

Vision is the user-facing feature; Bobby runs its local service on demand:

```bash
bobby vision status   # selected provider/model and service state
bobby vision start    # optional foreground run for debugging
```

Local agents already point at `bobby mcp-stdio` from install — no daemon.
Optional HTTP / CDP:

```bash
bobby serve           # http://127.0.0.1:7777/healthz (streamable HTTP MCP)
bobby cdp             # authenticated CDP on 127.0.0.1:9222 (dedicated port)
```

[CLI reference](docs/bobby-browser/source/pages/guides/cli.md) ·
[Run the server](docs/bobby-browser/source/pages/guides/run.md) (optional HTTP)

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

[`bobby-browser-client` on crates.io](https://crates.io/crates/bobby-browser-client) ·
[API docs on docs.rs](https://docs.rs/bobby-browser-client) ·
[Crate book](docs/bobby-browser/source/pages/rust/index.md)

## Published artifacts

One version, one `v*` tag, three artifacts:

| Artifact | Name |
|---|---|
| Binary | [GitHub release assets](https://github.com/cavi-ai/bobby-browser/releases/latest) — `bobby`, `mcp-gateway`, `acp-gateway` |
| Homebrew | [`cavi-ai/tap/bobby-browser`](https://github.com/cavi-ai/homebrew-tap) |
| npm | [`@cavi-ai/bobby-browser`](https://www.npmjs.com/package/@cavi-ai/bobby-browser) |
| crates.io | [`bobby-browser-client`](https://crates.io/crates/bobby-browser-client) — [docs.rs](https://docs.rs/bobby-browser-client) |

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
[`docs/bobby-browser/v0.8.0`](docs/bobby-browser/v0.8.0) for documentation hosts.
