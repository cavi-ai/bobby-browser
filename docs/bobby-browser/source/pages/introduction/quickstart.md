---
documentedVersion: 0.2.0
---

# Quickstart

## Run the server

```bash
cargo run -p cli -- serve
# or with an explicit config file:
BOBBY_BROWSER_CONFIG=/path/to/config.toml cargo run -p cli -- serve
```

Then open:

- `http://127.0.0.1:7777/healthz`
- `http://127.0.0.1:7777/runtime`

Keep the runtime on loopback or an operator-controlled boundary. Do not expose it to untrusted networks.

## TypeScript client

```ts
import { BrowserRuntimeClient } from "@bobby-browser/sdk";

const client = new BrowserRuntimeClient({
  baseUrl: "http://127.0.0.1:7777",
  bearerToken: process.env.AUTOMATION_RUNTIME_TOKEN!,
});
const info = await client.runtimeInfo();
```

HTTP calls require `Authorization`, the current `X-Interface-Version`, and a bounded correlation identifier. Mutating requests also require an idempotency key.

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
