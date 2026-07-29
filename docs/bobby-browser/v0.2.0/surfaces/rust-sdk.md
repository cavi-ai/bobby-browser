---
documentedVersion: 0.2.0
---

# Rust SDK

In-process and embedded control uses workspace crates rather than a single
published `bobby-browser` crates.io package (alpha; publishing is phased).

## Crate map

| Crate | Role |
|---|---|
| `cli` | `bobby` binary (`init`, `serve`, …) |
| `broker` | Authenticated HTTP `/v1/*` + MCP HTTP mount |
| `sdk-core` | Runtime interface implementation behind the broker |
| `interface-core` | Authority store, events, idempotency, authorization |
| `types` | Shared wire types, capabilities, commands, interface version |
| `config` | `AppConfig` / `config.toml` schema |
| `mcp-gateway` | MCP stdio process |
| `cdp-gateway` | Authenticated CDP discovery + DevTools WebSockets |
| `intent-engine` / `page-runtime` / `session-manager` / … | Execution engines |

Build the CLI: `cargo build -p cli`. Workspace tests: `cargo test --workspace`
(some live browser tests are ignored until Chromium is installed).

## Embed vs HTTP

- **HTTP / TypeScript / MCP HTTP** — talk to `bobby serve` with bearer + interface headers.
- **Embed** — construct `AuthorityStore` / runtime interfaces in-process (same
  capability model). Minimal authority sketch:

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

Beyond quickstart: issue least-privilege capability sets, re-check at dispatch,
prefer durable token records for multi-principal servers, and never log
plaintext bearers. HTTP issuance for remote principals is
`POST /v1/principals` with `authority:admin` — see
[Multi-principal](../concepts/multi-principal.md).

## Next

- [Quickstart](../introduction/quickstart.md)
- [HTTP API](http-api.md)
- [Capabilities](../concepts/capabilities.md)
