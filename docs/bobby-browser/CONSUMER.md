# bobby-browser Documentation Consumer Contract

This contract describes how a documentation host consumes the immutable
documentation artifact built for bobby-browser `0.2.0`.

```text
source: docs/bobby-browser/v0.2.0
publicBasePath: /docs/bobby-browser/v0.2.0
stableAlias: /docs/bobby-browser
entrypoints: manifest.json, navigation.json
identity: manifest.package / manifest.product
contentIntegrity: manifest.contentSha256
```

## Copy And Install

Copy the complete `docs/bobby-browser/v0.2.0` directory from this repository
checkout (or its CI documentation artifact) to the host's
`/docs/bobby-browser/v0.2.0` public base path. Serve `/docs/bobby-browser` as an
alias to that immutable version only after validation succeeds. Do not merge
files from another package version into this directory.

Use `manifest.json` to validate the artifact and `navigation.json` as the
navigation entry point. Paths in `navigation.json` are relative to the public
base path (page paths are relative to the version directory root).

## Integrity And Immutability

The manifest version must equal the documented version, `0.2.0`.
`manifest.package` / `manifest.product` must be `bobby-browser`.

Verify `manifest.contentSha256` by hashing every artifact file except
`manifest.json`, in lexical path order, as `path`, NUL, bytes, NUL. This digest
is the authority for generated-content integrity. There is no npm tarball
digest in v1.

Consumers must fail ingestion on a version or digest mismatch.
Consumers must not edit generated pages; replace the complete versioned
directory with a newly validated immutable artifact when upgrading.

## Build And Verify In This Repository

```bash
node scripts/docs/build-bobby-browser.mjs
node scripts/docs/verify-bobby-browser.mjs
node --test scripts/docs/bobby-browser-docs.test.mjs
```

Host-side route wiring for `/docs/bobby-browser` is outside this repository;
this contract is the ingest API.
