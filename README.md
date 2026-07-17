# Automation Runtime

A browser automation runtime with three control surfaces:

- Native SDK
- MCP server
- CDP compatibility layer

This scaffold implements the first thin vertical slice:

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
