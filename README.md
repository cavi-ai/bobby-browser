# Automation Runtime

A browser automation runtime with five authenticated control surfaces:

- Rust SDK
- TypeScript SDK over HTTP
- MCP over stdio
- Playwright over authenticated CDP
- Puppeteer over authenticated CDP

All adapters use the same capability, idempotency, evidence, checkpoint, and
event contracts. Authentication and authorization fail closed; credentials are
never accepted in URLs or query strings.

## Authentication bootstrap

The runtime enrolls a SHA-256 digest of a high-entropy bearer credential at
startup. Supply the plaintext credential only through a protected process input
or secret manager, then send it as `Authorization: Bearer <token>`. Never put a
token in a URL, command argument, config committed to source control, or log.
The examples below deliberately use the non-secret placeholder
`$AUTOMATION_RUNTIME_TOKEN`.

Tokens bind one principal to an explicit capability set and expiry. Revocation
and expiry are checked again at dispatch, including long-lived MCP and CDP
connections. Typical least-privilege capabilities include `session:read`,
`session:write`, `page:read`, `page:write`, `browser:mutate`, `file:upload`,
`file:download`, `artifact:read`, `artifact:capture`, `recovery:read`, and
`recovery:write`. JavaScript evaluation requires its own capability.

## Rust quick start

```rust,no_run
use chrono::{Duration, Utc};
use interface_core::AuthorityStore;
use types::{Capability, PrincipalId};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let authority = AuthorityStore::in_memory();
let issued = authority.issue(
    PrincipalId::from_uuid(uuid::Uuid::new_v4()),
    [Capability::SessionRead, Capability::SessionWrite],
    Utc::now() + Duration::minutes(5),
).await?;
let handle = authority.verify(&issued.expose_once()).await?;
let context = handle.context(Utc::now() + Duration::seconds(30), None);
# let _ = context;
# Ok(()) }
```

## TypeScript quick start

```ts
import { BrowserRuntimeClient } from "@bobby-browser/sdk";

const client = new BrowserRuntimeClient({
  baseUrl: "http://127.0.0.1:7777",
  bearerToken: process.env.AUTOMATION_RUNTIME_TOKEN!,
});
const info = await client.runtimeInfo();
```

The HTTP API requires `Authorization`, the current `X-Interface-Version`, and a
bounded correlation identifier. Mutating requests additionally require an
idempotency key. Duplicate or conflicting security-sensitive headers are
rejected; bodies are limited to 1 MiB by default.

## MCP stdio

Launch `mcp-gateway` with the credential delivered through its protected
startup channel. Configure an MCP client to run the binary directly; stdout is
reserved exclusively for newline-delimited JSON-RPC and diagnostics go to
stderr. The supported protocol version is `2025-11-25`; frames are limited to
1 MiB, tool input to 256 KiB, and event reads to 256 records. Initialize before
calling tools. Cancellation, EOF, expiry, and revocation close or reject work
without leaking credentials.

```json
{
  "mcpServers": {
    "automation-runtime": {
      "command": "mcp-gateway",
      "env": { "AUTOMATION_RUNTIME_TOKEN": "${AUTOMATION_RUNTIME_TOKEN}" }
    }
  }
}
```

## Authenticated CDP

Discovery is available at `/json/version` and `/json/list`; WebSockets use
`/devtools/browser/:id` and `/devtools/page/:id`. Every discovery request and
WebSocket upgrade must include `Authorization: Bearer <token>`. Connect with
Playwright `1.61.1` via `chromium.connectOverCDP(endpoint, { headers })`, or
Puppeteer `25.3.0` via `puppeteer.connect({ browserWSEndpoint, headers })`.
Connections are limited to 128 in-flight messages, 1024 queued events, and
1 MiB frames. Runtime identifiers are connection-scoped and worker-generation
aware.

The compiled allowlist, client coverage, and explicitly unsupported domains are
published in [`docs/cdp-support.json`](docs/cdp-support.json). Raw CDP
forwarding is intentionally unsupported. Features absent from that manifest,
arbitrary browser process control, unbounded streams, URL credentials, remote
filesystem paths, and implicit replay of uncertain boundary commands are not
supported.

## Events and recovery

Persist the last processed event cursor. Reconnect with that cursor to resume
exactly. If retention has advanced, the adapter returns an `EventGap` with
`historyLost` and `earliestAvailable`; restart from that cursor only after
re-reading durable session/checkpoint state. `invalidCursor` and `invalidLimit`
are caller errors. Never guess across a gap.

Replayable work may retry only through runtime policy. Reconciliable work is
inspected against checkpoint invariants. Any loss at accepted, prepared,
executing, verifying, or result-prepared boundaries that cannot prove the
outcome remains `NeedsReconciliation`; it is never silently replayed.

## Capacity and performance proof

Release gates exercise 64 authenticated clients, eight active workflows, 32
warm/resumable sessions, slow event consumers, and concurrent artifact reads.
Equivalent-work measurements use at least seven warmed samples per adapter and
report median/IQR for adapter overhead separately from browser time. Raw JSONL,
heap profiles, and CPU profiles belong under `benchmarks/raw/` and are ignored.
The concise summary printed by the performance gate is the reproducible record.

- broker startup
- in-memory session/page state
- typed domain models
- minimal HTTP health endpoint
- MCP/CDP placeholders

## Run

```bash
cargo run -p cli -- serve
```

Then open:

- `http://127.0.0.1:7777/healthz`
- `http://127.0.0.1:7777/runtime`

## Next steps

1. Replace placeholders with real engine implementations.
2. Add MCP stdio and Streamable HTTP.
3. Add CDP discovery and WebSocket routing.
4. Introduce V8-backed `js-engine`.

## Quality gates

The health smoke script is preliminary and does not prove browser behavior. The
vertical slice requires both the complete workspace suite and the live Chromium
workflow proof:

```bash
cargo test --workspace
cargo test -p runtime-tests --test browser_vertical_slice -- --ignored --nocapture
```

## Adaptive HTTP execution

`AppConfig::http` controls the direct HTTP path. `HttpConfig` defaults to denying
loopback and private-network destinations, follows at most five redirects, bounds
headers, decoded bodies, downloads, request duration, and shared concurrency. A
local fixture must be granted explicitly with `allow_loopback`; production should
leave both `allow_loopback` and `allow_private_network` disabled unless its threat
model and destination allowlist require otherwise. Redirect destinations are
validated independently, so a permitted origin does not grant a private redirect.

Only replayable static `Inspect` commands and explicit `DownloadUrl` commands are
eligible for direct HTTP. Semantic targets, mutations, boundary commands,
non-HTTP URLs, JavaScript-dependent documents, ambiguous cache/body state, and
unsupported content route to Chromium. Inspection uncertainty may fall back once
and records `chromiumFallback` evidence. Explicit downloads fail closed when
content type or equivalence cannot be proven; they are never replayed as an unsafe
Chromium download. Successful downloads are held as private pending artifacts,
browser HTTP state is committed at the exact observed version, and only then is
the artifact published for its owning session.

Every completed adaptive command includes `ExecutionPath` evidence. Operators can
distinguish `directHttp`, `chromium`, and `chromiumFallback`, along with the reason,
state version, elapsed time, byte count, and hash where applicable. The following
commands are the correctness and capacity proof; health and smoke checks remain
preliminary signals only. Capacity correctness is based on completion count, the
fixture-observed concurrency peak, and execution-path evidence. Performance is
reported separately from warmed sequential wall-clock samples for direct runtime
inspection versus Chromium navigation plus forced inspection; median ordering is
measurement evidence, not a correctness assertion.

```bash
cargo test -p runtime-tests --test adaptive_http_security -- --nocapture
cargo test -p runtime-tests --test adaptive_http_capacity -- --nocapture
cargo test -p runtime-tests --test adaptive_http adaptive_http -- --ignored --exact --nocapture
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p runtime-tests --test browser_vertical_slice completes_dynamic_form_with_durable_evidence -- --ignored --exact --nocapture
cargo test -p runtime-tests --test checkpoint_recovery replaces_chrome_then_resumes_or_restarts_from_verified_state -- --ignored --exact --nocapture
cargo test -p runtime-tests --test agentic_interaction completes_semantic_drift_frame_shadow_wait_and_artifact_workflow -- --ignored --exact --nocapture
```
