---
documentedVersion: {{PRODUCT_VERSION}}
---

# types

**Tier: Supported**

Shared wire types for HTTP, MCP, and embed paths: capabilities, command
envelopes (including primitives such as `activatePage` /
`AccessibilitySnapshot` and intents such as `CompleteForm` / `Fill`),
session/page state, recovery types, and `CURRENT_INTERFACE_VERSION`
(`{{INTERFACE_VERSION}}`).

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

## Schema feature

Enable Cargo feature `schema` to derive `schemars::JsonSchema` on the wire
types (`PrimitiveCommand`, `IntentCommand`, `Evidence`, …). The MCP gateway
keeps hand-bounded tool JSON Schemas in `crates/mcp-gateway/src/schema.rs` and
runs `schema_parity` tests so advertised `kind` variants cannot drift from
these Rust enums. Prefer regenerating or updating the hand schemas whenever
you add a command or evidence variant — the parity tests fail closed.

## Next

- [bobby-browser-client](bobby-browser-client.md)
- [Capabilities](../concepts/capabilities.md)
- [HTTP API](../surfaces/http-api.md)
- [MCP tools](../surfaces/mcp-tools.md)
