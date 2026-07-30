---
documentedVersion: 0.2.1
---

# Quickstart

## Run the server

```bash
cargo build -p bobby-browser
./target/debug/bobby init
./target/debug/bobby serve
```

Or without a pre-built binary:

```bash
cargo run -p bobby-browser -- init
cargo run -p bobby-browser -- serve
# or with an explicit config file:
BOBBY_BROWSER_CONFIG=/path/to/config.toml cargo run -p bobby-browser -- serve
```

Then open:

- `http://127.0.0.1:7777/healthz` — unauthenticated liveness
- Authenticated HTTP under `/v1/*` (for example `GET /v1/runtime`) — requires a bearer and interface headers (see [Authentication](../guides/auth.md))

There is no `/runtime` route. Use `/v1/runtime`.

On loopback, if no bootstrap env or secret file exists, `bobby serve` auto-generates
one and prints the bearer once. Export that value as `AUTOMATION_RUNTIME_TOKEN` for
SDK clients (same plaintext bearer). Keep the runtime on loopback or an
operator-controlled boundary. Do not expose it to untrusted networks.

## TypeScript client

```ts
import { BrowserRuntimeClient } from "@bobby-browser/sdk";

const client = new BrowserRuntimeClient({
  baseUrl: "http://127.0.0.1:7777",
  bearerToken: process.env.AUTOMATION_RUNTIME_TOKEN!,
});
const info = await client.runtimeInfo();
```

The SDK sets `Authorization`, `x-interface-version` (`2026-07-23`),
`x-correlation-id`, `x-deadline`, and `idempotency-key` on mutating POSTs. Raw
HTTP callers must send those headers themselves — see [Authentication](../guides/auth.md).

For a full session → page → navigate loop, see [First browser session](first-session.md).

## Rust authority sketch

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
