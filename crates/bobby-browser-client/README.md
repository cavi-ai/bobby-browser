# bobby-browser-client

Typed HTTP client and `/v1` wire types for a Bobby Browser runtime
(`bobby serve`). This is the Rust SDK published on crates.io.

```bash
cargo add bobby-browser-client
# optional JsonSchema derives on wire types:
cargo add bobby-browser-client --features schema
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
println!("{:?}", info.version);
# Ok(()) }
```

Every request sends `Authorization`, `x-interface-version`,
`x-correlation-id`, and `x-deadline`. The npm package
`@cavi-ai/bobby-browser` exposes the same HTTP surface.
