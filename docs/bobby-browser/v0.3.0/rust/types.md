---
documentedVersion: 0.3.0
---

# types

**Tier: Supported**

Shared wire types for HTTP, MCP, and embed paths: capabilities, command
envelopes (including primitives such as `activatePage` /
`AccessibilitySnapshot` and intents such as `CompleteForm` / `Fill`),
session/page state, recovery types, and `CURRENT_INTERFACE_VERSION`
(`2026-07-23`).

```bash
cargo add types
# workspace path today: crates/types
```

```rust,no_run
use types::{CreateSessionRequest, CURRENT_INTERFACE_VERSION};

let _ = CURRENT_INTERFACE_VERSION;
let _ = CreateSessionRequest {
    profile: "default".into(),
    proxy: None,
    execution_policy: Default::default(),
};
```

Prefer `types` for serde-compatible request/response bodies. Breaking changes
remain possible while the product is alpha — pin versions deliberately.

## Next

- [bobby-browser-client](bobby-browser-client.md)
- [Capabilities](../concepts/capabilities.md)
- [HTTP API](../surfaces/http-api.md)
