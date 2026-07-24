# Contributing

Thanks for contributing to bobby-browser. The project is in **alpha**; APIs may
still change before 1.0.

## Development setup

- Rust toolchain from `rust-toolchain.toml`
- Node.js 22+ and pnpm for TypeScript packages

```bash
cargo build -p cli
pnpm install
```

## Tests

Primary gate:

```bash
cargo test --workspace
```

Live Chromium vertical slice (when proving browser behavior):

```bash
cargo test -p runtime-tests --test browser_vertical_slice -- --ignored --nocapture
```

Additional release and companion proofs are documented in historical release notes
and scripts under `scripts/`; ask maintainers before treating ignored proofs as
required for a small docs or SDK change.

## Documentation package

Public technical docs are curated under `docs/bobby-browser/source/` and built
into an immutable versioned artifact for the landing-page docs host:

```bash
pnpm docs:build
pnpm docs:verify
pnpm docs:test
```

See `docs/bobby-browser/CONSUMER.md` for host ingest rules. Do not edit files
under `docs/bobby-browser/v*/` by hand — rebuild from source.

## Pull requests

- Prefer focused PRs with a clear problem statement
- Do not commit secrets, private absolute paths, or bearer tokens
- Link security issues privately per [SECURITY.md](SECURITY.md) — do not open
  public issues for vulnerabilities

## License

By contributing, you agree that your contributions are licensed under the MIT
License in [LICENSE](LICENSE).
