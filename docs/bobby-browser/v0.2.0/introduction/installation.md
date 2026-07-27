---
documentedVersion: 0.2.0
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
cargo build -p cli
pnpm install
```

## Bootstrap credential

Supply a high-entropy bearer through a protected process input or secret manager. The runtime enrolls a SHA-256 digest of that credential at startup. Never commit the plaintext token.

Use the non-secret placeholder `$AUTOMATION_RUNTIME_TOKEN` in examples only.
