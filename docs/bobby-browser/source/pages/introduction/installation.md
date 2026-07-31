---
documentedVersion: 0.3.0
---

# Installation

## Prerequisites

- Rust toolchain matching `rust-toolchain.toml`
- Node.js 22+ and pnpm for TypeScript packages
- Chromium (managed by the runtime for browser workflows) when running live browser proofs

## Clone and build

```bash
git clone https://github.com/cavi-ai/bobby-browser.git
cd bobby-browser
cargo build -p bobby-browser
pnpm install
```

The cargo package is `bobby-browser`; the binary is `bobby`:

```bash
./target/debug/bobby doctor
```

## Bootstrap credential

Run `bobby init` to generate a local bootstrap secret. It writes a dotenv file
with the four `AUTOMATION_RUNTIME_BOOTSTRAP_*` variables under the OS config
directory (`…/bobby-browser/bootstrap.env`, mode `0600` where supported). Override
the path with `BOBBY_BROWSER_BOOTSTRAP_ENV` or `bobby init --path <file>`.

```bash
./target/debug/bobby init
```

`bobby init` prints the plaintext bearer once. Map that value to
`AUTOMATION_RUNTIME_TOKEN` (or `Authorization: Bearer`) for SDK clients that use
the bootstrap principal directly. Never commit the plaintext token or put it in
`config.toml`.

You can also supply the same env contract through a protected process input or
secret manager. Use the non-secret placeholder `$AUTOMATION_RUNTIME_TOKEN` in
examples only.
