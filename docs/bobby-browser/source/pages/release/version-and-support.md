---
documentedVersion: {{PRODUCT_VERSION}}
---

# Version and support

Product docs line: **{{PRODUCT_VERSION}}**. Interface version string:
**`{{INTERFACE_VERSION}}`** (`CURRENT_INTERFACE_VERSION` / TypeScript `INTERFACE_VERSION`).

## Support expectations (alpha)

- Interfaces are stable enough to build against, but may change before 1.0.
- Public docs are published as the GitHub Release asset
  `bobby-browser-docs-v{{PRODUCT_VERSION}}.tar.gz` with an integrity manifest
  (`CONSUMER.md`), built from `docs/bobby-browser/source`.
- Registry publishes (npm / crates.io / Release binaries) may lag the git tag —
  verify with `npm view` / `cargo search` / GitHub Releases before documenting
  an install as live.

## Where to look

| Artifact | Location |
|---|---|
| Hosted docs | https://cavi-ai.xyz/docs/bobby-browser |
| Changelog | [CHANGELOG.md](https://github.com/cavi-ai/bobby-browser/blob/main/CHANGELOG.md) |
| Security policy | [SECURITY.md](https://github.com/cavi-ai/bobby-browser/blob/main/SECURITY.md) |
| CDP allowlist | [`docs/cdp-support.json`](https://github.com/cavi-ai/bobby-browser/blob/main/docs/cdp-support.json) |

## Next

- [Overview](../introduction/overview.md)
- [MIT license](license.md)
