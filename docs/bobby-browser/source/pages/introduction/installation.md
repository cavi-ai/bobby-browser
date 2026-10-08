---
documentedVersion: {{PRODUCT_VERSION}}
---

# Installation

Install the `bobby` command, then run `bobby install` to wire it to your agent host.

## Install the CLI

Pick one.

**Install script (Linux and macOS).** Installs `bobby`, `mcp-gateway` and `acp-gateway`.

```bash
curl -fsSL https://raw.githubusercontent.com/cavi-ai/bobby-browser/main/scripts/install.sh | bash
```

Set `BOBBY_VERSION` (for example `{{PRODUCT_VERSION}}`, no leading `v`) to pin a version and `INSTALL_DIR` to change the destination (default `~/.local/bin`). Rerun the script to upgrade. Files outside the managed binaries are kept.

**PowerShell (Windows x64).**

```powershell
irm https://raw.githubusercontent.com/cavi-ai/bobby-browser/main/scripts/install.ps1 | iex
```

**Homebrew.**

```bash
brew tap cavi-ai/tap
brew install cavi-ai/tap/bobby-browser
```

**Release archive.** Download `bobby-browser-<version>-<os>-<arch>.tar.gz` (`.zip` on Windows) from the GitHub Releases page, with `<os>` one of `linux`, `macos`, `windows` and `<arch>` one of `x64`, `arm64`. Each archive holds `bobby`, `mcp-gateway` and `acp-gateway`. Put them on your `PATH`.

**Source.** Requires the Rust toolchain pinned in `rust-toolchain.toml`, plus Node 22 and pnpm for the Firefox companion.

```bash
git clone https://github.com/cavi-ai/bobby-browser.git
cd bobby-browser
pnpm install
make install
```

`make install` builds `bobby`, `mcp-gateway` and `acp-gateway` in release mode, builds the companion extension, and runs `bobby install`. That puts the binaries on `PATH`, installs the companion, wires your agent host and creates a credential if none exists. It shows the checklist in a terminal and runs `bobby install --yes` without one. `make install RESTART=1` also stops the running runtime. `make cli` installs only the three binaries, and `make firefox` installs only the companion.

## Wire up your host

```bash
bobby install
bobby doctor
```

`bobby install` is an interactive checklist. It creates the bootstrap credential, writes the host configuration, and installs the agent skill. To skip the prompts, name the host:

```bash
bobby install --host claude --yes
```

| `--host` | Protocol | Configuration written |
|---|---|---|
| `claude` | MCP stdio | project `.mcp.json` |
| `vscode` | MCP stdio | project `.vscode/mcp.json` |
| `zed` | MCP stdio | `~/.config/zed/settings.json` |
| `acp` | ACP stdio | project `.acp.json` |
| `openshell` | MCP streamable HTTP | project `openshell/` pack |

Host entries launch `bobby mcp-stdio` or `bobby acp-stdio` and carry no credentials. `bobby doctor` reports stale entries and `bobby doctor --fix` repairs them.

Useful `bobby install` flags:

| Flag | Effect |
|---|---|
| `--companion` | Install the [Firefox companion](../guides/firefox-companion.md) |
| `--cli` | Copy `bobby`, `mcp-gateway` and `acp-gateway` onto `PATH` |
| `--skill`, `--skill-claude`, `--skill-openclaw`, `--skill-hermes` | Install the agent skill for the named agent |
| `--vision`, `--vision-provider <name>` | Enable [vision assist](../guides/configuration.md#vision) |
| `--force` | Regenerate the bootstrap credential |
| `--restart-runtime` | Stop the running runtime so the next agent connection starts the new build. Add `--disconnect-agents` to skip the prompt when agents are attached |

A running runtime keeps serving the build it started with until you restart it.

## Where files go

`bobby doctor` prints the resolved paths.

| What | Where |
|---|---|
| `config.toml` | OS config directory under `bobby-browser/`. `--config` or `BOBBY_BROWSER_CONFIG` overrides it |
| Bootstrap credential | `bootstrap.env` in the same directory (`~/Library/Application Support/bobby-browser/` on macOS) |
| CLI binaries | `~/.cargo/bin` if it is on `PATH`, else `~/.local/bin` |
| Agent skill | `~/.agents/skills/bobby-browser/`, plus `~/.claude/skills/` for Claude Code |
| OpenClaw skill | `$OPENCLAW_STATE_DIR/skills/bobby-browser/`, else `~/.openclaw/skills/` |
| Hermes skill | `$HERMES_HOME/skills/bobby-browser/`, else `~/.hermes/skills/` |

Credentials are never printed during `bobby install` or `bobby doctor`.

## Teams and projects

Add `--team <name>` and `--project <name>` before the subcommand to give a team or project its own profile and shared runtime:

```bash
bobby --team engineering --project checkout install --host claude --yes
bobby --team engineering --project checkout runtime status
```

## SDK packages

```bash
npm install @cavi-ai/bobby-browser
pip install bobby-browser
cargo add bobby-browser-client
```

The CLI is not published on crates.io.

## Create a credential by hand

`bobby install` creates the credential for you. To rotate it or write it elsewhere:

```bash
bobby init --force
bobby init --path ./bootstrap.env
```

`bobby init` prints the bearer once. See [Authentication](../guides/auth.md).

## Next

- [Quickstart](quickstart.md)
- [CLI reference](../guides/cli.md)
