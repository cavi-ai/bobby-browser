---
documentedVersion: 0.3.1
---

# interface-core

**Tier: Embed**

In-process authority store, capability checks, events, idempotency, and related
authorization primitives. Use when embedding the runtime rather than calling
HTTP.

Typical entry: `AuthorityStore::in_memory()` (or durable records) → `issue` /
`verify` → capability context for dispatch. See the sketch on
[Rust SDK](../surfaces/rust-sdk.md).

Prefer HTTP + `bobby-browser-client` for remote automation. Do not log
plaintext bearers; only hashes are durable for issued principals.

## Next

- [sdk-core](sdk-core.md)
- [Multi-principal](../concepts/multi-principal.md)
- [Capabilities](../concepts/capabilities.md)
