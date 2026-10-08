---
documentedVersion: {{PRODUCT_VERSION}}
---

# Rust SDK

`bobby-browser-client` is a typed async client for the [HTTP API](../surfaces/http-api.md), with the request and response types of the `/v1` interface.

```bash
cargo add bobby-browser-client
cargo add bobby-browser-client --features schema   # adds JsonSchema derives
```

## Connect

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
# let _ = info;
# Ok(()) }
```

Get the token with `bobby token`. The client sends the authorization, interface version, correlation ID and deadline headers on every request. The last argument of each method is optional per-call options such as an idempotency key.

## Methods

| Method | Route |
|---|---|
| `runtime_info` | `GET /v1/runtime` |
| `create_session`, `list_sessions`, `delete_session` | `/v1/sessions` |
| `open_page` | `POST /v1/pages` |
| `form_snapshot` | `GET /v1/sessions/{session}/pages/{page}/forms` |
| `submit` | `POST /v1/commands` |
| `context_ask`, `context_neighbors`, `context_site` | `GET /v1/context/*` |
| `checkpoint` | `POST /v1/checkpoints` |
| `recovery_status`, `recover` | `/v1/recovery/{workflow}` |
| `artifact` | `GET /v1/artifacts/{id}`, verified |
| `submit_job`, `job_status`, `cancel_job`, `resolve_job` | `/v1/jobs` |

`resolve_job(&job_id, &input, options)` records an operator attestation for a job whose outcome is uncertain. `input` carries `effectObserved` or `effectAbsent` and a lowercase hex SHA-256. The caller needs `job:read`, `job:cancel` and `authority:admin`. See [Events and recovery](../guides/events-recovery.md).

The crate re-exports the wire types, such as `CommandEnvelope` and `CURRENT_INTERFACE_VERSION`. The `bobby` CLI is not a crate dependency; install it as described in [Installation](../introduction/installation.md).

## Next

- [HTTP API](../surfaces/http-api.md)
- [Authentication](../guides/auth.md)
- [First session from code](../introduction/first-session.md)
