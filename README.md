# bobby-browser

A browser automation runtime with authenticated, capability-scoped control
surfaces:

- Rust SDK
- TypeScript SDK over HTTP
- MCP over stdio
- MCP over streamable HTTP (`POST /v1/mcp`) — the multi-tenant driver surface
- Playwright over authenticated CDP
- Puppeteer over authenticated CDP

All adapters use the same capability, idempotency, evidence, checkpoint, and
event contracts. Authentication and authorization fail closed; credentials are
never accepted in URLs or query strings. The runtime is **multi-principal**: a
single instance serves many independent tenants, each with its own
capability-scoped bearer token, per-principal in-flight quota, and a token store
that survives restart.

> **Alpha.** The interfaces and contracts described here are stable enough to
> build against, but may still change before 1.0. See
> [SECURITY.md](SECURITY.md) for the security model and reporting.

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
`file:download`, `artifact:read`, `artifact:capture`, `recovery:read`,
`recovery:write`, `javascript:evaluate`, and `authority:admin`.

## Multi-principal token issuance

The bootstrap credential holds `authority:admin` and is the only principal that
can mint or revoke other tokens, over an authenticated HTTP surface:

- `POST /v1/principals` issues a scoped bearer for a principal. Issuance is
  capability-bounded: a token cannot mint `authority:admin`, the issued
  capability set must be a **subset of the issuer's**, and the TTL is capped
  (90 days). The bearer is returned exactly once in the response body.
- `DELETE /v1/principals/{id}` revokes a principal; its bearers stop
  authenticating immediately.

Only SHA-256 hashes of issued bearers are persisted (atomic, owner-only writes),
so tokens survive a runtime restart while revoked/expired records are compacted
away. Each principal has an independent in-flight request quota
(`interface.max_in_flight_per_principal`), so one tenant's burst cannot starve
another. All issuance requests carry the standard authenticated headers
(`Authorization`, `X-Interface-Version`, a bounded correlation id, a deadline,
and an idempotency key for the mutating `POST`).

## JavaScript evaluation

Evaluating arbitrary JavaScript is **deny-by-default** and gated twice; both
gates must pass:

1. **Token capability** — the bearer must hold `javascript:evaluate`. Without
   it, an `evaluateJavaScript` command is rejected (`MissingCapability`) before
   any dispatch.
2. **Per-session execution policy** — the session must have been created with
   `executionPolicy.javascriptEvaluation = true`. A session created without an
   explicit grant (the default) rejects JavaScript with `PolicyDenied`, even if
   the token holds the capability. An unknown session fails closed.

Execution is bounded: the result is size-capped (`browser.max_js_result_bytes`)
and the run is time-capped (`browser.max_js_timeout_ms`). A successful run
returns a `javaScriptResult` evidence item.

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
    "bobby-browser": {
      "command": "mcp-gateway",
      "env": { "AUTOMATION_RUNTIME_TOKEN": "${AUTOMATION_RUNTIME_TOKEN}" }
    }
  }
}
```

## MCP over streamable HTTP

For multi-tenant use, the served runtime exposes the same MCP tool surface over
streamable HTTP at `POST /v1/mcp` with bearer-only auth — the driver surface for
any harness that speaks MCP (Claude Code, Codex, OpenClaw, …). Each tenant needs
only a URL and its scoped token; nothing is shipped to the client. One JSON-RPC
message per `POST`; `GET` is unsupported. Server state is isolated per principal,
and a rotated token resets that principal's MCP lifecycle (re-`initialize`).

```json
{
  "mcpServers": {
    "bobby-browser": {
      "url": "http://127.0.0.1:7777/v1/mcp",
      "transport": "streamable-http",
      "headers": { "Authorization": "Bearer ${BOBBY_BROWSER_TOKEN}" }
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

Release gates exercise 64 authenticated persistent connections; overflow fails
immediately with typed HTTP `429 ResourceExhausted` and `Retry-After: 1`. They
bound overload responses to `interface.max_rejection_workers` (default 16);
connections beyond that pool close immediately without spawning per-peer work.
The gates also prove FIFO admission for eight active workflows across 32 warm/resumable
sessions, slow event consumers, and an `ArtifactReader` maximum of eight
concurrent reads whose overload is typed retryable with a 25 ms retry delay.
Equivalent-work measurements use one discarded warmup followed by seven paired
samples on a persistent fixture for each adapter. They report adapter-operation
time, adapter wall time, their harness-envelope delta, and process-tree
RSS before/at peak/after the adapter closes its real transport. Raw JSONL, heap
profiles, and CPU profiles belong under `benchmarks/raw/` and are ignored. Run
`pnpm --filter @bobby-browser/interface-conformance test:release`; the concise
summary printed by that release gate is the reproducible record.

## Configuration

`serve` loads `./config.toml` at startup, overridable with the
`BOBBY_BROWSER_CONFIG` environment variable. A missing file uses built-in
defaults; a malformed or invalid file fails startup loudly with the offending
path named. The committed [`config.toml`](config.toml) documents every field and
mirrors the `AppConfig` schema (`server`, `browser`, `storage`, `http`,
`interface`). The bootstrap credential is supplied separately through the
`AUTOMATION_RUNTIME_BOOTSTRAP_*` environment variables, never the config file.

## Run

```bash
cargo run -p cli -- serve
# or with an explicit config file:
BOBBY_BROWSER_CONFIG=/path/to/config.toml cargo run -p cli -- serve
```

Then open:

- `http://127.0.0.1:7777/healthz`
- `http://127.0.0.1:7777/runtime`

## Quality gates

The health smoke script is preliminary and does not prove browser behavior. The
vertical slice requires both the complete workspace suite and the live Chromium
workflow proof:

```bash
cargo test --workspace
cargo test -p runtime-tests --test browser_vertical_slice -- --ignored --nocapture
```

### Installed Firefox companion proof

The native-browser release proof uses a headed installed Firefox, a dedicated
test profile, the built companion extension, the loopback companion coordinator,
and Firefox WebDriver BiDi as one workflow. The launcher atomically installs the
non-secret `com.bobby_browser.companion` Native Messaging manifest and an
owner-only repo-local wrapper. The wrapper passes the hardened dynamic descriptor
path explicitly to the native host; Firefox does not need a test-only environment
variable. Existing different wrapper or manifest files are never overwritten.
Do not use a personal profile: the proof temporarily links the unpacked extension
into the specified profile and removes only that owned link during cleanup.

From the repository root, run exactly:

```bash
BOBBY_FIREFOX_BIN="/Applications/Firefox.app/Contents/MacOS/firefox" \
BOBBY_FIREFOX_PROFILE="/absolute/path/to/dedicated-test-profile" \
BOBBY_COMPANION_EXTENSION="$(pwd)/packages/firefox-companion/dist" \
./scripts/dev/firefox-companion.sh
```

The launcher fails closed when any required variable or path is absent, builds
the Rust native host and extension, installs the Native Messaging manifest in
Firefox's per-user manifest directory, and invokes only the exact ignored
installed Firefox test. Set `BOBBY_NATIVE_MESSAGING_DIR` to an alternate absolute
directory for isolated setup/testing. Proof state is bounded to
`target/firefox-companion-proof`; profile contents and pairing bearer material
are never printed. A passing proof records Firefox
and profile identity, verified navigate/inspect/click/typeText operations,
engine-native click and typing, the exact `Submitted` confirmation, bounded
timing, and zero redaction findings. Deterministic tests do not substitute for
this headed proof.

## Security release certification

Run the authoritative security certification from the repository root:

```bash
cargo run -p release-gates -- security \
  --manifest config/release-gates.json \
  --output target/release-gates/security.json
```

This command requires an installed Chromium browser and permission to use its
local loopback fixtures. Certification persistence is supported on Unix only;
other platforms fail closed before running checks or creating output. The
manifest must be a regular file no larger than 64 KiB; it is opened once,
bounded, hashed, and parsed through that same descriptor. The immutable catalog
requires exact Cargo test counts and one unique proof marker per check, including
the installed-Chromium capacity fixture. Zero-test, filtered/missing expected
proof, ignored required proof, malformed receipt, or count/marker mismatch blocks
the release.

The bounded JSON bundle contains the manifest digest, compiled-catalog digest,
the complete ordered required result set, the policy-recomputed verdict, and a
canonical bundle digest. These SHA-256 values detect corruption and prove
internal content consistency; they are not signatures and do not authenticate a
bundle against an actor who can rewrite and rehash it. Key management and signed
attestation are outside this certification phase. Health and smoke tests remain
preliminary signals and are not substitutes for the live certification.

The deterministic policy fixtures cover IPv4/IPv6 link-local and cloud-metadata
destinations, mixed-address DNS rebinding, and redirect/navigation re-evaluation.
CDP target admission proves popup pages remain page-scoped while worker and
service-worker target families are not exposed through the gateway. The catalog
also runs the authenticated interface boundary matrix and connection/workflow
capacity proofs; certification cannot pass when any required family receipt is
missing or ignored.

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
