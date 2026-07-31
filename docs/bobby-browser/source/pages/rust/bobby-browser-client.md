---
documentedVersion: {{PRODUCT_VERSION}}
---

# bobby-browser-client

**Tier: Supported**

Typed HTTP client for a running `bobby serve` instance. Mirrors
`@bobby-browser/sdk` (`BrowserRuntimeClient`) for the common `/v1` path.

```bash
cargo add bobby-browser-client
# or from this repo:
cargo test -p bobby-browser-client
```

```rust,no_run
use bobby_browser_client::BrowserRuntimeClient;
use types::CreateSessionRequest;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let client = BrowserRuntimeClient::new(
    "http://127.0.0.1:7777",
    std::env::var("AUTOMATION_RUNTIME_TOKEN")?,
)?;
let info = client.runtime_info(None).await?;
let session = client
    .create_session(
        &CreateSessionRequest {
            profile: "default".into(),
            proxy: None,
            execution_policy: Default::default(),
        },
        None,
    )
    .await?;
client.delete_session(&session.id, None).await?;
let _ = info;
# Ok(()) }
```

Every request sends `Authorization: Bearer …`, `x-interface-version`,
`x-correlation-id`, and `x-deadline`. See [Authentication](../guides/auth.md).

## Surface

| Method | HTTP |
|---|---|
| `runtime_info` | `GET /v1/runtime` |
| `create_session` / `list_sessions` / `delete_session` | sessions |
| `open_page` | `POST /v1/pages` |
| `submit` | `POST /v1/commands` |

Events, artifacts, checkpoints, and recovery helpers may still be missing
versus the TypeScript SDK — use raw HTTP or extend the crate when needed.

## Next

- [types](types.md)
- [HTTP API](../surfaces/http-api.md)
- [Rust SDK overview](../surfaces/rust-sdk.md)
