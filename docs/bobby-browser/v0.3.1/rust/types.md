---
documentedVersion: 0.3.1
---

# Wire types

**Tier: Supported** (via `bobby-browser-client`)

`/v1` Serde wire types ship inside
[`bobby-browser-client`](bobby-browser-client.md) on crates.io
(`CreateSessionRequest`, command envelopes, outcomes, forms, recovery,
`CURRENT_INTERFACE_VERSION` = `2026-07-23`).

```bash
cargo add bobby-browser-client
```

```rust,no_run
use bobby_browser_client::{CreateSessionRequest, CURRENT_INTERFACE_VERSION};

let _ = CURRENT_INTERFACE_VERSION;
let _ = CreateSessionRequest {
    profile: "default".into(),
    proxy: None,
    execution_policy: Default::default(),
};
```

The workspace still has an internal `types` crate (`publish = false`) for
runtime crates; do not publish or `cargo add` it.

## Schema feature

```bash
cargo add bobby-browser-client --features schema
```

## Next

- [bobby-browser-client](bobby-browser-client.md)
- [Capabilities](../concepts/capabilities.md)
- [HTTP API](../surfaces/http-api.md)
