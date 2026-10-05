---
documentedVersion: {{PRODUCT_VERSION}}
---

# Rust SDK

Two ways to use Rust with bobby-browser:

1. **Remote HTTP** — [`bobby-browser-client`](../rust/bobby-browser-client.md)
   (Supported tier), mirroring `@cavi-ai/bobby-browser`
2. **Embed** — workspace crates such as `interface-core` / `sdk-core` / `broker`
   (Embed tier) — see the [Rust crate book](../rust/index.md)

The CLI (`bobby`) is not on crates.io: install it from a release binary,
Homebrew, or source ([Installation](../introduction/installation.md)).

## HTTP client (recommended for apps)

```rust,no_run
use bobby_browser_client::BrowserRuntimeClient;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let client = BrowserRuntimeClient::new(
    "http://127.0.0.1:7777",
    std::env::var("AUTOMATION_RUNTIME_TOKEN")?,
)?;
let info = client.runtime_info(None).await?;
let _ = info;
# Ok(()) }
```

## Crate map (summary)

| Crate | Tier | Role |
|---|---|---|
| `bobby-browser-client` | Supported | HTTP `/v1` client **and** wire types (crates.io SDK) |
| `bobby-browser` | Supported (CLI) | `bobby` binary; lib name `cli` |
| `interface-core` / `sdk-core` / `broker` | Embed | In-process runtime |
| engines / stores / `types` | Internal | Workspace only (`types` is `publish = false`) |

Full tiered list: [Rust crate book](../rust/index.md).

Build the CLI: `cargo build -p bobby-browser`. Workspace tests:
`cargo test --workspace`.

## Embed sketch

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

- [Rust crate book](../rust/index.md)
- [CLI reference](../guides/cli.md)
- [HTTP API](http-api.md)
- [Capabilities](../concepts/capabilities.md)

## Resolving uncertain jobs

`resolve_job(&job_id, &input, options)` records an owner-scoped operator attestation for a
reconciliation-required job. Supply `effectObserved` or `effectAbsent` with a
64-character lowercase evidence SHA-256. The owner needs `job:read`,
`job:cancel`, and `authority:admin`. The receipt has `operatorAttested`
provenance, is validated against the request, and never replays the handler.
See [Events and recovery](../guides/events-recovery.md) for degraded storage
and retention semantics.
