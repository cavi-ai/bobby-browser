---
documentedVersion: 0.14.0
---

# bobby-browser-client

**Tier: Supported**

The Rust SDK on crates.io: typed HTTP client plus `/v1` wire types for a
running `bobby serve` instance. Mirrors `@cavi-ai/bobby-browser`.

```bash
cargo add bobby-browser-client
cargo add bobby-browser-client --features schema   # optional JsonSchema
```

```rust,no_run
use bobby_browser_client::{BrowserRuntimeClient, CreateSessionRequest};

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
| `form_snapshot` | `GET /v1/sessions/{session}/pages/{page}/forms` |
| `context_ask` / `context_neighbors` / `context_site` | context reads |
| `submit` | `POST /v1/commands` |
| `checkpoint` | `POST /v1/checkpoints` |
| `recovery_status` / `recover` | recovery |
| `artifact` | verified `GET /v1/artifacts/{id}` |

Wire types (`CreateSessionRequest`, `CommandEnvelope`, `CURRENT_INTERFACE_VERSION`,
…) are re-exported from this crate.

## Next

- [HTTP API](../surfaces/http-api.md)
- [Rust SDK overview](../surfaces/rust-sdk.md)
