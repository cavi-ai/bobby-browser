# bobby-browser

Give your agents the browser work.

![Bobby Browser: a cartoon bee takes on a stack of browser tasks.](assets/bobby-browser-hero.png)

[![Release](https://img.shields.io/github/v/release/cavi-ai/bobby-browser)](https://github.com/cavi-ai/bobby-browser/releases/latest)
[![TypeScript SDK downloads per week](https://img.shields.io/npm/dw/@cavi-ai/bobby-browser?label=TypeScript%20SDK%2Fweek)](https://www.npmjs.com/package/@cavi-ai/bobby-browser)
[![Python SDK downloads per week](https://img.shields.io/pypi/dw/bobby-browser?label=Python%20SDK%2Fweek)](https://pypi.org/project/bobby-browser/)
[![Rust SDK](https://img.shields.io/crates/v/bobby-browser-client?label=Rust%20SDK)](https://crates.io/crates/bobby-browser-client)
[![License: MIT](https://img.shields.io/github/license/cavi-ai/bobby-browser)](LICENSE)

Bobby automates Firefox and Chromium for AI agents and applications. Turn recurring web tasks into reusable workflows, retain useful site structure and continue interrupted work from recorded progress.

## Browser work you can build on

- **Remember the site.** With a durable browser profile, Bobby remembers control names, roles and verified targets across visits. Agents can ask for a relevant control before requesting a full page snapshot.
- **Reuse the workflow.** Combine browser steps into a named workflow with inputs and outputs. Run it with new inputs; inspect the step results when something needs attention.
- **Recover your progress.** Command records and evidence-backed checkpoints help you inspect interrupted work and decide where to continue. Check an uncertain submission on the destination before sending it again.

Use MCP, HTTP, ACP, authenticated CDP, or the TypeScript, Python and Rust SDKs. These interfaces share the same runtime and command pipeline.

## Start with a useful task

After setup, give your agent the pages and the result you want:

> Compare these three product pages. Make a table with the model name, listed price, availability and source URL. Leave missing information blank and keep the original currency.

For a recurring job, have the agent turn the observed steps into a [named workflow](docs/bobby-browser/source/pages/guides/workflows.md). You supply the inputs and desired result; the agent operates the browser.

For forms, ask the agent to fill the fields and show their state before submitting. Submission is a separate step: check the confirmation the actual site returns. See [intent commands](docs/bobby-browser/source/pages/guides/intents.md).

## Benchmark

Bobby against other agent browser tools on five sign-in-gated journeys (cookie banner, sign-in and authenticator code, then the task), with Claude Sonnet 5 driving each tool, 5 runs per tool. All four tools completed every journey.

| Bobby vs | Speed | Tokens | Ease of use (1–5) |
| --- | --- | --- | --- |
| Microsoft Playwright MCP | Same | 44% fewer | 4.8 vs 2.2 |
| Google Chrome DevTools MCP | 12% faster | 58% fewer | 4.8 vs 1.5 |
| Vercel agent-browser | 46% faster | 52% fewer | 4.8 vs 2.2 |

The agent driving each tool rated Bobby easiest to use: 4.8 of 5, against 1.5 to 2.2 for the others.

Measured with Bobby's competitor benchmark at commit `1dc7b2a` against `@playwright/mcp@0.0.79`, `chrome-devtools-mcp@1.7.0` and `agent-browser@0.37.1`. Tokens are uncached input plus output; ease is the driving model's 1–5 rating for navigate, click, fill and extract.

## Get started

Install the runtime with Homebrew:

```bash
brew tap cavi-ai/tap
brew install cavi-ai/tap/bobby-browser
bobby install
bobby doctor
```

The setup checklist creates a credential and connects your agent host. Firefox requires [one-time companion installation and pairing](docs/bobby-browser/source/pages/guides/firefox-companion.md).

Restart your agent host, then give it the task above.

[Linux, Windows, release archives and source installation](docs/bobby-browser/source/pages/introduction/installation.md) · [First browser session](docs/bobby-browser/source/pages/introduction/quickstart.md)

After an upgrade, restart the running runtime to activate the new build. See [installation](docs/bobby-browser/source/pages/introduction/installation.md).

## Keep the work connected

[Site memory](docs/bobby-browser/source/pages/concepts/context-graph.md) retains page structure and verified target records per durable browser profile, without storing typed values, credentials, page text or screenshots.

[Workflow files](docs/bobby-browser/source/pages/guides/workflows.md) support inputs, conditions, repeated steps and outputs. A stopped file workflow can continue from a reached step on the same connection, reusing its earlier recorded results.

[Events and recovery](docs/bobby-browser/source/pages/guides/events-recovery.md) cover durable checkpoints and resume, restart or reconciliation decisions after an interruption.

[Scoped callers](docs/bobby-browser/source/pages/concepts/multi-principal.md) let one runtime serve multiple agents and applications with separate sessions, pages and request quotas.

## Use from code

The SDKs connect to an installed Bobby runtime.

| Language | Package | Guide |
| --- | --- | --- |
| TypeScript | `npm install @cavi-ai/bobby-browser` | [TypeScript SDK](docs/bobby-browser/source/pages/surfaces/typescript-sdk.md) |
| Python | `pip install bobby-browser` | [Python SDK](docs/bobby-browser/source/pages/surfaces/python-sdk.md) |
| Rust | `cargo add bobby-browser-client` | [Rust SDK](docs/bobby-browser/source/pages/rust/index.md) |

Start with [a session from code](docs/bobby-browser/source/pages/introduction/first-session.md), the [HTTP API](docs/bobby-browser/source/pages/surfaces/http-api.md), or [authenticated CDP](docs/bobby-browser/source/pages/surfaces/cdp.md).

The [Cavi API client example](examples/cavi-api-client/README.md) reads page text with Bobby and streams a summary through your selected model provider.

## Access and permissions

Calls are authenticated and checked against the caller's capabilities. Side-effecting calls use idempotency keys; command evidence and checkpoints support inspection and recovery. Credentials are never accepted in URLs.

See the [security model](docs/bobby-browser/source/pages/security/model.md), [capabilities](docs/bobby-browser/source/pages/concepts/capabilities.md) and [SECURITY.md](SECURITY.md).

## Documentation and help

Full documentation: [cavi-ai.xyz/docs/bobby-browser](https://cavi-ai.xyz/docs/bobby-browser).

- [Overview](docs/bobby-browser/source/pages/introduction/overview.md), [configuration](docs/bobby-browser/source/pages/guides/configuration.md) and [CLI](docs/bobby-browser/source/pages/guides/cli.md)
- [MCP tools](docs/bobby-browser/source/pages/surfaces/mcp-tools.md) and [CDP support](docs/cdp-support.json)
- [Evidence and checkpoints](docs/bobby-browser/source/pages/concepts/evidence-checkpoints.md)
- [Troubleshooting](docs/bobby-browser/source/pages/guides/troubleshooting.md)
- [Report an issue](https://github.com/cavi-ai/bobby-browser/issues) or [contribute](CONTRIBUTING.md)

Bobby is in alpha; interfaces may change before 1.0. Released builds are available on [GitHub Releases](https://github.com/cavi-ai/bobby-browser/releases).

## Development

```bash
make build
make test
make lint
```

See [CONTRIBUTING.md](CONTRIBUTING.md) and [TESTING.md](TESTING.md).

## License

[MIT](LICENSE).
