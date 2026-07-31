---
documentedVersion: 0.3.0
---

# bobby-browser-client

**Tier: Supported**

Typed HTTP client for a running `bobby serve` instance. Mirrors
`@bobby-browser/sdk` (`BrowserRuntimeClient`).

```bash
cargo add bobby-browser-client
# or from this repo:
cargo test -p bobby-browser-client
```

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

Every request sends `Authorization: Bearer …`, `x-interface-version`,
`x-correlation-id`, and `x-deadline`. See [Authentication](../guides/auth.md).

## Surface

- `runtime_info`
- `create_session` / `list_sessions`
- `open_page`
- `submit` (`CommandEnvelope`)

More TS methods (events, artifacts, recovery) may land in later releases.

## Next

- [types](types.md)
- [HTTP API](../surfaces/http-api.md)
- [Rust SDK overview](../surfaces/rust-sdk.md)
