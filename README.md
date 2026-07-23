# Automation Runtime

Automation Runtime is an early-stage Rust service for coordinating browser-automation sessions and pages behind a common runtime API. The intended control surfaces are:

- a native SDK
- an MCP server
- a Chrome DevTools Protocol (CDP) compatibility layer

The current vertical slice provides an HTTP broker, in-memory session and page state, shared domain types, and placeholder MCP/CDP crates. It does **not** launch or control a browser yet: navigation currently updates page state without making a network request.

## Prerequisites

- A recent stable Rust toolchain with Cargo
- `curl` for the smoke script and command-line examples

## Run the service

From the repository root:

```bash
cargo run -p cli -- serve
```

The broker listens on `127.0.0.1:7777`. Host and port are currently fixed by `AppConfig::default`.

Useful read-only endpoints:

```bash
curl http://127.0.0.1:7777/healthz
curl http://127.0.0.1:7777/runtime
curl http://127.0.0.1:7777/sessions
```

The CLI also exposes a lightweight diagnostic command:

```bash
cargo run -p cli -- doctor
```

It prints `ok` when the binary starts successfully.

## Try the session flow

With the service running, create a session:

```bash
curl -X POST http://127.0.0.1:7777/sessions \
  -H 'content-type: application/json' \
  -d '{"profile":"default","proxy":null}'
```

Use the returned `id` value to open a page:

```bash
curl -X POST http://127.0.0.1:7777/pages \
  -H 'content-type: application/json' \
  -d '{"session_id":"<session-id>"}'
```

Then use the page `id` to record a navigation:

```bash
curl -X POST http://127.0.0.1:7777/navigate \
  -H 'content-type: application/json' \
  -d '{"page_id":"<page-id>","url":"https://example.com","wait_until":null,"timeout_ms":null}'
```

For the same flow in TypeScript, see [`examples/typescript-sdk/example.ts`](examples/typescript-sdk/example.ts). The example uses the built-in `fetch` API and expects the broker to already be running.

## HTTP API

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/healthz` | Return `{ "ok": true }` when the broker is serving requests. |
| `GET` | `/runtime` | Report the runtime version, advertised capabilities, session count, queued jobs, and uptime placeholder. |
| `GET` | `/sessions` | List the sessions held by this broker process. |
| `POST` | `/sessions` | Create a session from `profile` and optional `proxy` fields. |
| `POST` | `/pages` | Create a page in an existing session. |
| `POST` | `/navigate` | Update an existing page with a URL and `interactive` ready state. |

IDs are serialized as UUID strings. State is process-local and is lost when the service exits. Runtime errors are returned as JSON objects with an `error` field; HTTP status mapping has not been implemented yet.

## Workspace architecture

| Crate | Responsibility | Current status |
| --- | --- | --- |
| `cli` | Process entry point and `serve`/`doctor` commands | Working vertical slice |
| `broker` | Axum HTTP routes and shared application state | Working vertical slice |
| `sdk-core` | Runtime service facade over sessions and pages | Working vertical slice |
| `types` | Shared request, response, ID, state, and error types | Working vertical slice |
| `session-manager` | In-memory session lifecycle | Working vertical slice |
| `page-runtime` | In-memory page lifecycle and URL state | Working vertical slice |
| `config` | Server configuration types and defaults | Defaults only |
| `network-engine`, `dom-engine`, `js-engine`, `artifact-store`, `worker-pool`, `tracing` | Planned runtime subsystems | Scaffolded |
| `mcp-gateway`, `cdp-gateway` | Planned protocol adapters | Placeholders |

## Verify the workspace

Run the Rust test suite:

```bash
cargo test --workspace
```

Run the basic live-service smoke check:

```bash
./scripts/dev/smoke.sh
```

The smoke script starts the broker, checks `/healthz` and `/runtime`, and then stops the process. It is a startup check, not an end-to-end browser automation test.

## Current limitations

- Sessions and pages exist only in memory.
- Navigation records a URL but does not fetch or render it.
- `wait_until` and `timeout_ms` are accepted but not acted on.
- MCP and CDP transports are not implemented.
- Runtime uptime and queued-job metrics are placeholders.
- Server configuration is not yet loaded from files, flags, or environment variables.

## Planned work

1. Connect page navigation to real browser and network engines.
2. Add MCP stdio and Streamable HTTP transports.
3. Add CDP discovery and WebSocket routing.
4. Introduce a V8-backed JavaScript engine.
5. Add persistence, job execution, artifacts, and production-ready error/status handling.
