---
documentedVersion: 0.8.0
---

# sdk-core

**Tier: Embed**

Runtime service implementation behind the broker: sessions, pages, command
dispatch into engines, evidence assembly. Not a remote HTTP client — use
`bobby-browser-client` for that.

Embedding path (simplified): construct config + authority + runtime service,
then either serve via `broker` or call the runtime interfaces in-process. Keep
capability checks at every dispatch boundary.

## Next

- [broker](broker.md)
- [interface-core](interface-core.md)
- [Engines and stores](engines.md)
