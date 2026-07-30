# Contributing

Thanks for contributing to bobby-browser. The project is in **alpha**; APIs may
still change before 1.0.

## Development setup

- Rust toolchain from `rust-toolchain.toml`
- Node.js 22+ and pnpm for TypeScript packages

```bash
cargo build -p bobby-browser
./target/debug/bobby doctor
pnpm install
```

The CLI binary is `bobby` (cargo package `cli`). Common commands: `bobby init`,
`bobby serve`, `bobby doctor`.

## Tests

Primary gate:

```bash
cargo test --workspace
```

Live Chromium vertical slice (when proving browser behavior):

```bash
cargo test -p runtime-tests --test browser_vertical_slice -- --ignored --nocapture
```

Further release and companion proofs run from `scripts/dev/`. `#[ignore]`d tests
require a real browser and are not gates for docs or SDK changes.

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

Participation is governed by the [Code of Conduct](CODE_OF_CONDUCT.md).

- Prefer focused PRs with a clear problem statement
- Do not commit secrets, private absolute paths, or bearer tokens
- Link security issues privately per [SECURITY.md](SECURITY.md) — do not open
  public issues for vulnerabilities

## License

By contributing, you agree that your contributions are licensed under the MIT
License in [LICENSE](LICENSE).
