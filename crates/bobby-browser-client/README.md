# bobby-browser-client

Typed HTTP client for a running Bobby Browser runtime (`bobby serve`).

```bash
cargo add bobby-browser-client
```

```rust,no_run
use bobby_browser_client::BrowserRuntimeClient;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let client = BrowserRuntimeClient::new(
    "http://127.0.0.1:7777",
    std::env::var("AUTOMATION_RUNTIME_TOKEN")?,
)?;
let info = client.runtime_info(None).await?;
println!("{info:?}");
# Ok(()) }
```

Requires the same auth headers contract as `@cavi-ai/bobby-browser`: bearer token,
`x-interface-version`, `x-correlation-id`, and `x-deadline`.
