---
documentedVersion: 0.7.0
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

Exit code `1` if any **fail** checks; warnings alone exit `0`.

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

## Next

- [Run the server](run.md)
- [Configuration](configuration.md)
- [First browser session](../introduction/first-session.md)
