---
documentedVersion: 0.8.0
---

# CLI reference

The `bobby` binary (Cargo package `bobby-browser`) is the primary way to run the
runtime locally.

```bash
cargo build -p bobby-browser --release
./target/release/bobby --help
./target/release/bobby --version
```

With no subcommand, `bobby` defaults to `serve`.

## Commands

### `bobby init`

Generate a loopback bootstrap credential (dotenv file, mode `0600` where
supported).

| Flag | Meaning |
|---|---|
| `--force` | Overwrite an existing bootstrap file |
| `--ttl-days <n>` | Expiry in days (default from CLI) |
| `--path <file>` | Bootstrap file path (else `BOBBY_BROWSER_BOOTSTRAP_ENV` / OS config dir) |

Prints the plaintext bearer **once**. Map it to `AUTOMATION_RUNTIME_TOKEN` for
SDK clients. Never commit the bearer or put it in `config.toml`.

### `bobby serve`

Start the authenticated HTTP broker (and MCP HTTP mount).

| Flag | Meaning |
|---|---|
| `--config <path>` | `config.toml` path (else `BOBBY_BROWSER_CONFIG`, else `./config.toml`) |
| `--bootstrap-env <path>` | Bootstrap dotenv path (else `BOBBY_BROWSER_BOOTSTRAP_ENV`, else default) |

On loopback, if no bootstrap credential exists, `serve` may generate one and
print the bearer once. Non-loopback binds require credentials up front.

Optional: `AUTOMATION_RUNTIME_BROWSER_SELECTION` (JSON) overrides engine
selection. Without it, the selection persisted by
`bobby enroll-firefox-profile` is used; default engine preference is Firefox.
The same resolution order applies to the MCP gateway and `bobby doctor` —
see [Configuration](configuration.md).

Health: `GET http://<host>:<port>/healthz`.

### `bobby doctor`

Local setup checks (config load, browser selection, engine satisfiability,
bootstrap presence, storage dirs, Firefox/Chromium on PATH, optional `/healthz`).

| Flag | Meaning |
|---|---|
| `--config <path>` | Same as `serve` |
| `--bootstrap-env <path>` | Same as `serve` |
| `--skip-health` | Do not probe `/healthz` (default: probe) |
| `--fix` | Repair safe Bobby-owned state, readiness-test the selected provider, then run doctor again |
| `--download-model` | With `--fix`, explicitly allow downloading the already-selected MLX model |

Exit code `1` if any **fail** checks; warnings alone exit `0`.

In an interactive terminal, `ok`, `warn`, and `fail` are green, yellow, and
red. Repair results use cyan, green, yellow, or red according to outcome.
Piped output and `NO_COLOR=1 bobby doctor` remain plain and keep the same text
labels, so color is never required to understand a result.

`--fix` is conservative and idempotent. It can heal an existing unrestricted
bootstrap capability set, normalize the selected provider into Bobby's
canonical vision node, and readiness-test that selected provider. It does not
choose a provider/model, overwrite a custom endpoint, persist secrets, install
system packages, or leave a daemon running. A missing MLX cache remains an
action item unless `--download-model` gives explicit consent for the download.

### `bobby jobs`

HTTP client for the broker job API (`/v1/jobs`). The scheduler runs
**in-process inside `bobby serve`** — the CLI does not start a second scheduler.

Bootstrap credentials need `job:submit`, `job:read`, and `job:cancel`. New
`bobby init` / loopback serve credentials include these by default. Existing
`bootstrap.env` files are not migrated; run `bobby init --force` (or enroll a
principal with `job:*`) before using these commands.

```bash
bobby jobs submit --name echo --payload '{"message":"hi"}'
bobby jobs submit --name echo --payload-file ./job.json --priority high \
  --idempotency-key run-1
bobby jobs status <job_id>
bobby jobs cancel <job_id>
```

Shared flags on all `jobs` subcommands:

| Flag | Meaning |
|---|---|
| `--config <path>` | Same as `serve` |
| `--bootstrap-env <path>` | Same as `serve` (bearer source if no token env) |
| `--base-url <url>` | Override `http://{host}:{port}` from config |
| `--token <bearer>` | Override `AUTOMATION_RUNTIME_TOKEN` / bootstrap bearer |

`submit` flags: `--name` (required), `--payload` (JSON string, default `{}`),
`--payload-file`, `--priority` (`low|normal|high|critical`, default `normal`),
`--max-retries`, `--timeout-ms`, `--idempotency-key`.

### `bobby vision`

Vision provider setup. `connect` writes a provider profile into `config.toml`,
`login` establishes or verifies the configured ACP harness login, and `collect`
gathers training data from gauntlet runs.

```bash
bobby vision connect --yes --provider mlx
bobby vision connect --yes --provider mlx --activate --download-model
bobby vision connect --yes --backend acp --provider codex \
  --command codex --arg acp --auth advertised
bobby vision login
```

`bobby install` also offers vision configuration during onboarding. Field
reference: [Configuration](configuration.md#vision).

An explicitly selected provider is persisted before a bounded readiness test.
For MLX, Bobby loads the exact selected model through the same managed command
used at runtime and stops the setup-time child after the probe. Ollama and LM
Studio remain externally managed; onboarding reports how to start/load them
when their configured endpoint is unavailable.

`vision connect` remains configuration-only by default. Add `--activate` to
load/readiness-test the selection immediately. For MLX, add
`--download-model` only when Bobby may download the selected model if its cache
is missing; that flag requires `--activate`.

### `bobby vision-proxy`

Run the loopback vision proxy that serves `propose` / `extract` against an
upstream provider.

| Flag | Default | Meaning |
|---|---|---|
| `--bind <addr>` | `127.0.0.1:9100` | Bind address |
| `--path <path>` | `/vision` | HTTP path for the propose/extract POST |
| `--upstream <kind>` | `openai` | `openai`, `ollama`, or `mlx` |
| `--model <id>` | per upstream | Model id passed to the upstream |
| `--vision-base-url <url>` | per upstream | Upstream base URL |
| `--spawn-server` | off | Run the canonical Python vision server as a managed child (`mlx` upstream); the child is killed when the proxy exits |
| `--server-script <path>` | auto-detect | `vision_server.py` path when spawning (else `BOBBY_VISION_SERVER_SCRIPT`) |
| `--api-key-env <var>` | `OPENAI_API_KEY` | Env var holding the upstream API key; an empty value skips the key |
| `--collect-training-data` | off | Log vision proposals to disk |
| `--training-data-dir <dir>` | `data/vision/` | Destination for those logs |

`bobby serve --vision` starts the proxy alongside the runtime.

### `bobby openshell`

NVIDIA OpenShell host: write the pack, and mint or revoke one agent-scoped
principal per sandbox.

| Subcommand | Meaning |
|---|---|
| `install` | Write the `openshell/` pack (policy, `mcp.json`, skill, README) |
| `provision --sandbox <id>` | Mint one agent-scoped principal and write its injection env at mode 0600 |
| `rotate --sandbox <id>` | Revoke the prior principal and mint a fresh one |
| `list` | List locally recorded sandboxes (no secrets) |
| `status --sandbox <id>` | Non-secret status for one sandbox |
| `revoke --sandbox <id>` | Revoke the principal provisioned for a sandbox |

`bobby install --host openshell` writes the same pack, `bobby init --emit
openshell` prints the MCP fragment, and `bobby doctor` reports `openshell-pack`
and the related checks when a pack is present. See
[OpenShell](openshell.md).

### Firefox companion

```bash
bobby firefox-native-host --descriptor /abs/path/descriptor.json
bobby install-firefox-native-host \
  --wrapper /abs/wrapper \
  --manifest /abs/manifest.json \
  --cli /abs/bobby \
  --descriptor /abs/descriptor.json
bobby enroll-firefox-profile \
  --descriptor /abs/descriptor.json \
  --bidi-url ws://127.0.0.1:9222/session \
  --profile-dir /abs/profile
```

See [Firefox companion](firefox-companion.md).

## Environment

| Variable | Role |
|---|---|
| `BOBBY_BROWSER_CONFIG` | Default config path |
| `BOBBY_BROWSER_BOOTSTRAP_ENV` | Default bootstrap dotenv path |
| `AUTOMATION_RUNTIME_BOOTSTRAP_*` | Direct bootstrap env contract (see [Authentication](auth.md)) |
| `AUTOMATION_RUNTIME_TOKEN` | Client bearer (SDK / curl) |
| `AUTOMATION_RUNTIME_BROWSER_SELECTION` | JSON engine/profile selection override (else the persisted enrollment, else the Firefox default) |
| `BOBBY_MCP_TOOLSET` | Startup MCP phase, overriding `[mcp] startup_toolset` |
| `BOBBY_OPENSHELL_SECRETS_DIR` | Root for per-sandbox OpenShell injection env files (default: OS config dir) |
| `BOBBY_VISION_SERVER_SCRIPT` | `vision_server.py` path for `vision-proxy --spawn-server` |

## Next

- [Run the server](run.md)
- [Configuration](configuration.md)
- [First browser session](../introduction/first-session.md)
