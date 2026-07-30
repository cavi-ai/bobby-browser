# bobby-browser Documentation Consumer Contract

This contract describes how a documentation host consumes the immutable
documentation artifact built for bobby-browser `0.2.1`.

```text
source: GitHub Release asset bobby-browser-docs-v0.2.1.tar.gz
publicBasePath: /docs/bobby-browser/v0.2.1
stableAlias: /docs/bobby-browser
entrypoints: manifest.json, navigation.json
identity: manifest.package / manifest.product
contentIntegrity: manifest.contentSha256
releaseIntegrity: envelope.artifact.sha256
releaseProvenance: manifest.release.tag / manifest.release.commit
```

## Copy And Install

Extract the complete `docs/` directory from the immutable GitHub Release asset
after validating the schema-v1 `cavi-oss-release` envelope and archive SHA-256, then copy it to the host's
`/docs/bobby-browser/v0.2.1` public base path. Serve `/docs/bobby-browser` as an
alias to that immutable version only after validation succeeds. Do not merge
files from another package version into this directory.

Use `manifest.json` to validate the artifact and `navigation.json` as the
navigation entry point. Paths in `navigation.json` are relative to the public
base path (page paths are relative to the version directory root).

## Integrity And Immutability

The manifest version must equal the documented version, `0.2.1`.
`manifest.package` / `manifest.product` must be `bobby-browser`.

Verify `manifest.contentSha256` by hashing every artifact file except
`manifest.json`, in lexical path order, as `path`, NUL, bytes, NUL. This digest
is the authority for generated-content integrity. There is no npm tarball
digest in v1.

Consumers must fail ingestion on a version, tag, commit, repository, or digest mismatch.
Consumers must not edit generated pages; replace the complete versioned
directory with a newly validated immutable artifact when upgrading.

## Build And Verify In This Repository

```bash
pnpm docs:build
pnpm docs:verify
pnpm docs:test
```

Published releases require `CONSUMER_DISPATCH_TOKEN` and dispatch the verified
artifact envelope to cavi-home. Manual dry runs never upload or dispatch.
Historical backfills require an identical local dry run and explicit approval.
