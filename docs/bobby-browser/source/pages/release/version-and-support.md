---
documentedVersion: {{PRODUCT_VERSION}}
---

# Version and support

These docs describe bobby-browser **{{PRODUCT_VERSION}}**. The interface version is **`{{INTERFACE_VERSION}}`**, the value of the `x-interface-version` header, the Rust `CURRENT_INTERFACE_VERSION` constant and the TypeScript `INTERFACE_VERSION` constant.

## Stability

bobby is alpha. Interfaces are stable enough to build against but can change before 1.0. Read the changelog before upgrading.

## Releases

Each release publishes the CLI archives, the TypeScript, Python and Rust SDKs, and these docs as the GitHub Release asset `bobby-browser-docs-v{{PRODUCT_VERSION}}.tar.gz`. A registry can trail the git tag by a few minutes. Check `npm view @cavi-ai/bobby-browser`, `pip index versions bobby-browser` or `cargo search bobby-browser-client` for the version a registry serves.

## Where to look

| Artifact | Location |
|---|---|
| Hosted docs | https://cavi-ai.xyz/docs/bobby-browser |
| Changelog | [CHANGELOG.md](https://github.com/cavi-ai/bobby-browser/blob/main/CHANGELOG.md) |
| Security policy | [SECURITY.md](https://github.com/cavi-ai/bobby-browser/blob/main/SECURITY.md) |
| CDP method allowlist | [`docs/cdp-support.json`](https://github.com/cavi-ai/bobby-browser/blob/main/docs/cdp-support.json) |

## Next

- [Overview](../introduction/overview.md)
- [MIT license](license.md)
