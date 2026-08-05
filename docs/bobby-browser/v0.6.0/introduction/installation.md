---
documentedVersion: 0.6.0
---

# Installation

## Prerequisites

- Rust toolchain matching `rust-toolchain.toml`
- Node.js 22+ and pnpm for TypeScript packages (SDK / docs)
- Firefox (default engine) and/or Chromium when running live browser workflows

## Build from source (always works)

```bash
git clone https://github.com/cavi-ai/bobby-browser.git
cd bobby-browser
cargo build -p bobby-browser --release
./target/release/bobby doctor
./target/release/bobby --help
```

Package name: `bobby-browser`. Binary name: `bobby`.

## Install from registries (when published)

```bash
# CLI — after crates.io publish succeeds for this version
cargo install bobby-browser

# TypeScript SDK — after npm publish succeeds
npm install @cavi-ai/bobby-browser

# Rust HTTP client — after crates.io publish
cargo add bobby-browser-client
```

Do not treat registry installs as available until `npm view` / `cargo search`
shows the version you need. GitHub Release binaries (when cut) unpack to a
`bobby` binary; see the repository Releases page.

## Bootstrap credential

```bash
./target/release/bobby init
# or: bobby init --path ./bootstrap.env
```

Writes a dotenv with `AUTOMATION_RUNTIME_BOOTSTRAP_*` under the OS config dir
(or `--path`). Prints the plaintext bearer once — map it to
`AUTOMATION_RUNTIME_TOKEN` for clients. Never commit secrets into `config.toml`.

## Next

- [CLI reference](../guides/cli.md)
- [Quickstart](quickstart.md)
- [First browser session](first-session.md)
