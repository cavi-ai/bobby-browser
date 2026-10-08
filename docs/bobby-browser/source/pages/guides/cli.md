---
documentedVersion: {{PRODUCT_VERSION}}
---

# CLI

`bobby` installs, configures and runs the runtime. With no subcommand it runs `bobby serve`. `bobby --version` prints the version and `bobby <command> --help` lists a command's flags.

## Global options

| Option | Effect |
|---|---|
| `--team <name>` | Share a profile and runtime with this local team |
| `--project <name>` | Share a project runtime, optionally within a team |

Put them before the subcommand. Without them, bobby uses the personal scope.

## Commands

| Command | Purpose |
|---|---|
| `install` (alias `setup`) | Credential, host configuration, agent skill, companion, vision |
| `init` | Create or print a bootstrap credential |
| `token` | Print the current bearer |
| `doctor` | Check the local setup |
| `serve` | Run the HTTP API and MCP-over-HTTP endpoint |
| `cdp` | Run the runtime with authenticated CDP |
| `mcp-stdio`, `acp-stdio` | Gateways that agent hosts launch |
| `runtime` | Manage the shared local runtime |
| `firefox-start` | Open the Bobby Firefox profile |
| `enroll-firefox-profile`, `install-firefox-native-host`, `firefox-native-host` | Firefox companion plumbing |
| `profiles` | List deployment profiles |
| `jobs` | Submit, inspect and cancel jobs |
| `openshell` | OpenShell pack and sandbox principals |
| `context` | Inspect or erase remembered site context |
| `audit` | Export, verify and replay audit bundles |
| `vision` | Vision provider setup and diagnostics |

### `bobby install`

Interactive setup. Add `--yes` to accept defaults and `--host <claude|vscode|zed|acp|openshell>` (repeatable) to choose hosts without prompts. See [Installation](../introduction/installation.md) for the flag list.

### `bobby init`

Writes a credential file (mode 0600) and prints the bearer once.

| Flag | Meaning |
|---|---|
| `--force` | Overwrite an existing file |
| `--ttl-days <n>` | Expiry (default 30) |
| `--path <file>` | File path. Default is `bootstrap.env` in the config directory |
| `--preset <name>` | Capability floor: `agent` (default), `unrestricted`, `claude`, `codex`, `openshell`. See [Capabilities](../concepts/capabilities.md#generated-preset-matrix) |
| `--emit <host>` | Print an MCP client fragment for `claude`, `zed`, `vscode`, `json` or `openshell` |

### `bobby token`

Prints the enrolled bearer for SDK, HTTP and CDP clients. It refuses to write to a redirected stdout unless you pass `--stdout`.

### `bobby serve`

| Flag | Meaning |
|---|---|
| `--config <path>` | `config.toml` to load |
| `--bootstrap-env <path>` | Credential file |
| `--vision`, `--no-vision` | Start or skip the managed vision proxy |

On loopback, `serve` creates a credential if none exists and prints it once. Non-loopback binds need one up front. Health is at `GET /healthz`. `bobby mcp-stdio` and `bobby acp-stdio` take the same flags. `bobby cdp` also takes `--cdp-port`. See [Run the server](run.md).

### `bobby doctor`

Read-only checks of configuration, browser selection, credential, storage, sidecars, browsers on `PATH` and `/healthz`.

| Flag | Meaning |
|---|---|
| `--config`, `--bootstrap-env` | Same as `serve` |
| `--skip-health` | Do not probe `/healthz` or `GET /v1/runtime` |
| `--json` | Versioned JSON report on stdout |
| `--profile <name>` | Validate `desktop`, `headless-ci`, `openshell` or `remote` |
| `--fix` | Repair bobby-owned state, then run again |
| `--download-model` | With `--fix`, allow downloading the selected MLX model |
| `--downgrade-idempotency` | Convert healthy idempotency ledgers for use by older binaries |

Exit status is 1 when any check fails and 0 for warnings only. An unhealthy report starts with `next: bobby doctor --fix`, and repairable failures name the fix. Output is plain text when piped or when `NO_COLOR` is set.

`--fix` is idempotent. It restores an unrestricted credential's capabilities, creates missing storage directories, normalizes the selected vision provider, starts a loopback Ollama if the selected provider is down, and readiness-tests the provider. It never chooses a provider or model, overwrites a custom endpoint, stores secrets, installs system packages or leaves a daemon running. A missing MLX model stays an action item until you pass `--download-model`.

### `bobby runtime`

Manage the shared runtime of the current scope.

| Subcommand | Effect |
|---|---|
| `start` | Start or reuse the runtime |
| `status` | Show the owner and connection URL, and any pending change |
| `stop` | Stop the runtime gracefully |
| `restart` | Stop if running, then start |
| `list` | List local scopes and their status |

`stop` and `restart` ask before disconnecting attached agents. Without a terminal, pass `--disconnect-agents`.

### `bobby firefox-start`

Opens the installed Bobby Firefox profile for pairing or browsing. See [Firefox companion](firefox-companion.md).

### `bobby jobs`

An HTTP client for `/v1/jobs`. The scheduler runs inside `bobby serve`.

```bash
bobby jobs submit --name echo --payload '{"message":"hi"}'
bobby jobs submit --name echo --payload-file ./job.json --priority high --idempotency-key run-1
bobby jobs status <job_id>
bobby jobs cancel <job_id>
```

Shared flags: `--config`, `--bootstrap-env`, `--base-url <url>` and `--token <bearer>`. `submit` takes `--name` (required), `--payload`, `--payload-file`, `--priority` (`low`, `normal`, `high`, `critical`), `--max-retries`, `--timeout-ms` and `--idempotency-key`. The credential needs `job:submit`, `job:read` and `job:cancel`.

### `bobby audit`

Signed audit bundles for one workflow. See [Evidence and checkpoints](../concepts/evidence-checkpoints.md#audit-bundles).

```bash
bobby audit key
bobby audit export --workflow <workflowId> --out bundle.tar
bobby audit verify bundle.tar --public-key <hex>
bobby audit replay bundle.tar --out bundle.html
```

`export` takes `--config` and `--key <path>` (default `audit-signing-key.pk8` in the config directory, created on first use). `--out` must not exist. `verify` exits non-zero and names the failing file or signature. `replay` verifies and writes a self-contained page; see [Workflow replay](replay.md).

### `bobby context`

```bash
bobby context list --profile <profile-id>
bobby context forget --profile <profile-id> <site>
```

`list` shows remembered sites. `forget` erases everything remembered for one site. Both take `--config` and `--dir`. See [Context graph](../concepts/context-graph.md).

### `bobby vision`

| Subcommand | Purpose |
|---|---|
| `connect` | Write a provider profile to `config.toml` |
| `login` | Establish or verify the configured ACP harness login |
| `status` | Show the provider, model and service state |
| `start` | Run the vision service in the foreground |
| `detect` | Classify a challenge without acting |
| `solve` | Run the solve loop on a challenge |
| `collect` | Collect training data from gauntlet runs |

```bash
bobby vision connect --yes --provider mlx
bobby vision connect --yes --provider mlx --activate --download-model
bobby vision connect --yes --backend acp --provider codex --command codex --arg acp --auth advertised
```

`connect` flags: `--provider` (`openai`, `ollama`, `lmstudio`, `mlx`, `custom`), `--backend` (`direct` or `acp`), `--base-url`, `--model`, `--api-key-env`, `--command`, `--arg`, `--auth`, `--config`, `--yes`, `--activate`, `--download-model`. By default `connect` only writes configuration. `--activate` also readiness-tests the provider, and `--download-model` (which needs `--activate`) lets it fetch a missing MLX model. See [Configuration](configuration.md#vision).

`detect` and `solve` take `--purpose` and either `--url` for a new session or `--session` with `--page` for an existing one. `detect` is read-only and defaults to a 15 second budget (`--timeout-ms`). `solve` defaults to 120 seconds and accepts `--zigzagzig` for humanized input and fingerprint spoofing. `--node` picks the vision node (default `vision`). Both need `vision:assist`.

```bash
bobby vision detect --purpose "check for a captcha blocking signup" --url https://example.com
bobby vision solve --purpose "solve the challenge" --session <id> --page <id>
```

### `bobby openshell`

`install`, `provision`, `rotate`, `list`, `status` and `revoke`. See [OpenShell host](openshell.md).

## Environment variables

| Variable | Role |
|---|---|
| `BOBBY_BROWSER_CONFIG` | Default `config.toml` path |
| `BOBBY_BROWSER_BOOTSTRAP_ENV` | Default credential file path |
| `AUTOMATION_RUNTIME_BOOTSTRAP_*` | Direct credential input. See [Authentication](auth.md) |
| `AUTOMATION_RUNTIME_TOKEN` | Client bearer for SDKs and curl |
| `AUTOMATION_RUNTIME_BROWSER_SELECTION` | JSON engine selection, overriding the paired profile |
| `BOBBY_MCP_TOOLSET` | Starting MCP toolset, overriding `[mcp] startup_toolset` |
| `BOBBY_CHROME_EXECUTABLE` | Chromium binary to use |
| `BOBBY_OPENSHELL_SECRETS_DIR` | Directory for per-sandbox OpenShell environment files |
