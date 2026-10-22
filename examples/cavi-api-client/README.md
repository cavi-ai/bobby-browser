# Page summaries with the Cavi API client

This Node.js example uses Bobby to navigate to a caller-selected URL and read an
accessibility snapshot. The published `@cavi-ai/api-client` streams a short
summary through Claude, Codex, OpenClaw, Hermes, OpenCode or AGY. Bobby's runtime
does not need a model provider or a new dependency.

## Run

Requires Node 22+, pnpm, a running `bobby serve`, and access to a model provider.
Run these commands from the repository root:

```bash
pnpm install --frozen-lockfile
pnpm --filter @cavi-ai/bobby-browser build
pnpm --filter @cavi-ai/bobby-page-summary-example build

export BOBBY_BASE_URL=http://127.0.0.1:7777
export AUTOMATION_RUNTIME_TOKEN="$(bobby token)"
export CAVI_PROVIDER=codex
export CAVI_API_KEY="$OPENAI_API_KEY"
export CAVI_MODEL=your-model-id

pnpm --filter @cavi-ai/bobby-page-summary-example start https://example.com
```

The result is one JSON line: `{"text":"…","truncated":false}`. `truncated` means
the runtime or the example limited the page context. An accessibility snapshot
is a partial view of a page even when this flag is false.

The Bobby token needs `session:write`, `page:write`, `page:read`,
`browser:mutate` and `network:egress`. The example creates a session, opens a page,
navigates once, reads a snapshot, and deletes its session. It does not click,
fill forms, evaluate JavaScript or expose browser tools to the model.

## Choose a provider

All providers use the same `CapabilityClient.streamRun` call. Configuration is
local to the example; the Bobby SDK and runtime stay provider independent.

| `CAVI_PROVIDER` | Required configuration | Optional configuration |
|---|---|---|
| `claude` | `CAVI_API_KEY`, `CAVI_MODEL` | `CAVI_BASE_URL` |
| `codex` | `CAVI_API_KEY`, `CAVI_MODEL` | `CAVI_BASE_URL` |
| `openclaw` | `CAVI_BASE_URL`, `CAVI_API_KEY` (gateway token), `CAVI_SESSION_KEY` | — |
| `hermes` | `CAVI_BASE_URL`, `CAVI_API_KEY` (gateway token), `CAVI_SESSION_KEY` | — |
| `opencode` | `CAVI_BASE_URL`, `CAVI_DIRECTORY` (server project directory) | `CAVI_MODEL`, `CAVI_USERNAME`, `CAVI_PASSWORD` |
| `agy` | `CAVI_BASE_URL` | `CAVI_API_KEY`, `CAVI_MODEL` |

For gateway providers, choose a separate session key for the summary and an
agent configured without tools or approvals. OpenClaw uses a key such as
`agent:main:page-summary-<unique-id>`; use the session-key format your Hermes
gateway accepts. The API client owns SSE/WebSocket transport normalization.
OpenCode uses the API client's supported `opencode serve` HTTP/SSE protocol;
its project directory is explicit and is not inferred from this checkout.

## Data and failure handling

Use pages whose text may be shared with the selected provider. The prompt
contains only selected accessible names and roles, with at most 200 inspected
nodes, 256 Unicode characters per name and 8 KiB of serialized page context.
It omits raw form values, descriptions, targets, URLs, screenshots, identifiers
and credentials. Accessible names can themselves contain sensitive text; this
projection is not a sensitive-data classifier. The CLI bounds Bobby's JSON
response at 1 MiB.

Page text is untrusted input. Instructions tell the model to summarize it and
ignore embedded instructions; model output is never executed. The CLI emits
JSON so terminal control characters remain escaped. Tool and approval events,
missing or mismatched browser evidence, incomplete model runs, empty output
and output over 8 KiB fail the call. A terminal output snapshot takes precedence
over streamed deltas.

The CLI has a 60-second overall deadline and passes cancellation to both
clients. Cleanup has a separate five-second timeout. Cancellation requests
provider cancellation when supported; it does not prove that a remote run has
stopped. Failed calls are not automatically retried. A lost session-creation
acknowledgment can leave a session to reconcile through Bobby's session list.
Model-provider session retention follows the provider's policy; disposing the
client closes transports, not retained remote history.

## Use with an existing page

Import `summarizePage` from `src/summarize.ts` in a TypeScript project, or from
`dist/src/summarize.js` after building. Pass a `BrowserRuntimeClient`, a Cavi
`CapabilityClient`, the owned `sessionId` and `pageId`, and optionally a
`modelSessionKey`, `signal` or `timeoutMs` (1–60,000). The function only reads the
page; the caller owns session cleanup and `model.dispose()`.

## Test

```bash
pnpm --filter @cavi-ai/bobby-browser build
pnpm --filter @cavi-ai/bobby-page-summary-example test
```

Tests include local HTTP/SSE fixtures using the published Cavi client. They do
not launch a browser or contact paid providers.
