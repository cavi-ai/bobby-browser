---
documentedVersion: 0.8.0
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
shows the version you need.

## Install a GitHub Release binary

One-liner (Linux / macOS) — installs `bobby`, `mcp-gateway`, and `acp-gateway`
into `INSTALL_DIR` (default `~/.local/bin`):

```bash
curl -fsSL https://raw.githubusercontent.com/cavi-ai/bobby-browser/main/scripts/install.sh | bash
```

Optional: `BOBBY_VERSION=0.8.0` (no leading `v`) and `INSTALL_DIR=~/.local/bin`.

### Homebrew (macOS / Linuxbrew)

Not on [homebrew-core](https://github.com/Homebrew/homebrew-core) yet. From a
checkout of this repo:

```bash
brew tap cavi-ai/tap
brew install cavi-ai/tap/bobby-browser
```

From the tap, once it is published:

```bash
brew tap cavi-ai/tap
brew install cavi-ai/tap/bobby-browser
```

The tap repository is `cavi-ai/homebrew-tap`; brew strips the `homebrew-`
prefix and addresses it as `cavi-ai/tap`, so the formula is reached as
`cavi-ai/tap/bobby-browser` rather than repeating the project name.

The formula downloads the matching GitHub Release tarball and installs the
same three binaries.

When submitting to homebrew-core later: formula `bobby-browser`; bottle
`bobby` / `mcp-gateway` / `acp-gateway`; livecheck on GitHub Releases; CI that
builds all three; pass `brew audit --strict`.

### Manual download

Assets are named
`bobby-browser-<version>-{linux|macos|windows}-{x64|arm64}.tar.gz` (`.zip` on
Windows). Each archive contains `bobby`, `mcp-gateway`, and `acp-gateway`.
Example for the latest macOS arm64 release:

```bash
TAG="$(curl -fsSL https://api.github.com/repos/cavi-ai/bobby-browser/releases/latest | python3 -c 'import json,sys; print(json.load(sys.stdin)["tag_name"])')"
VERSION="${TAG#v}"
curl -fsSL -o bobby.tgz \
  "https://github.com/cavi-ai/bobby-browser/releases/download/${TAG}/bobby-browser-${VERSION}-macos-arm64.tar.gz"
tar -xzf bobby.tgz
STAGE="bobby-browser-${VERSION}-macos-arm64"
install -m 755 "$STAGE/bobby" "$STAGE/mcp-gateway" "$STAGE/acp-gateway" ~/.local/bin/
bobby doctor
```

Pick `linux-x64`, `linux-arm64`, `macos-x64`, or `windows-x64` to match your
host. Release archives are stripped on Unix; see the repository Releases page
for every asset.

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
