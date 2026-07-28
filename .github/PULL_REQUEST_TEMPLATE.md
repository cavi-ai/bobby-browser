## What changed

-

## Why

-

## Verification

- [ ] `cargo test --workspace`
- [ ] `cargo clippy --workspace --all-targets` clean
- [ ] `cargo fmt --all --check` clean
- [ ] TypeScript packages tested, if touched
- [ ] `pnpm docs:build && pnpm docs:verify && pnpm docs:test`, if docs touched
- [ ] Documentation release workflow dry-run output reviewed, if release delivery changed

## Contract impact

- [ ] No wire contract change
- [ ] Wire contract changed and `schemaVersion` handling updated
