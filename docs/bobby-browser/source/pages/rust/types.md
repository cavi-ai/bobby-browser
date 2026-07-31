---
documentedVersion: 0.3.0
---

# types

**Tier: Supported**

Shared wire types: capabilities, command envelopes, session/page state,
`CURRENT_INTERFACE_VERSION` (`2026-07-23`).

```bash
cargo add types
# workspace path today: crates/types
```

Prefer depending on `types` for serde-compatible request/response bodies when
speaking HTTP or embedding. Breaking changes remain possible while the product
is alpha.

## Next

- [bobby-browser-client](bobby-browser-client.md)
- [Capabilities](../concepts/capabilities.md)
