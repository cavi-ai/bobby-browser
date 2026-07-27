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

**Documentation:** [Overview](docs/bobby-browser/source/pages/introduction/overview.md) ·
[Quick start](docs/bobby-browser/source/pages/introduction/quickstart.md) ·
[Docs consumer contract](docs/bobby-browser/CONSUMER.md) ·
[Contributing](CONTRIBUTING.md)

## Quick start

Supply a high-entropy bootstrap credential through a protected process input or
secret manager (never commit it). Examples use the non-secret placeholder
`$AUTOMATION_RUNTIME_TOKEN`.

```bash
cargo run -p cli -- serve
# optional: BOBBY_BROWSER_CONFIG=/path/to/config.toml cargo run -p cli -- serve
```

Then open:

- `http://127.0.0.1:7777/healthz`
- `http://127.0.0.1:7777/runtime`

Keep the runtime on loopback or an operator-controlled boundary.

### TypeScript

```ts
import { BrowserRuntimeClient } from "@bobby-browser/sdk";

const client = new BrowserRuntimeClient({
  baseUrl: "http://127.0.0.1:7777",
  bearerToken: process.env.AUTOMATION_RUNTIME_TOKEN!,
});
const info = await client.runtimeInfo();
```

### Rust authority sketch

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

## Learn more

- [Authentication](docs/bobby-browser/source/pages/guides/auth.md)
- [JavaScript evaluation](docs/bobby-browser/source/pages/guides/javascript-eval.md)
- [Intents](docs/bobby-browser/source/pages/guides/intents.md)
- [Events and recovery](docs/bobby-browser/source/pages/guides/events-recovery.md)
- [Configuration](docs/bobby-browser/source/pages/guides/configuration.md)
- [MCP over HTTP](docs/bobby-browser/source/pages/surfaces/mcp-http.md) ·
  [MCP over stdio](docs/bobby-browser/source/pages/surfaces/mcp-stdio.md) ·
  [CDP](docs/bobby-browser/source/pages/surfaces/cdp.md)
- [Capabilities](docs/bobby-browser/source/pages/concepts/capabilities.md) ·
  [Evidence and checkpoints](docs/bobby-browser/source/pages/concepts/evidence-checkpoints.md) ·
  [Multi-principal](docs/bobby-browser/source/pages/concepts/multi-principal.md)

The CDP allowlist is published in
[`docs/cdp-support.json`](docs/cdp-support.json). The same pages are built into
an immutable versioned artifact under
[`docs/bobby-browser/v0.2.0`](docs/bobby-browser/v0.2.0) for documentation hosts.
