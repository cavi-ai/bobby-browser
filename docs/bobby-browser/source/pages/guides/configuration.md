---
documentedVersion: 0.2.0
---

# Configuration

`bobby serve` loads `./config.toml` at startup, overridable with
`BOBBY_BROWSER_CONFIG`. A missing file uses built-in defaults; a malformed file
fails startup and names the path.

The committed
[`config.toml`](https://github.com/cavi-ai/bobby-browser/blob/main/config.toml)
documents every field and mirrors `AppConfig` (`crates/config`).

## Common overrides

| Section | Fields | Default idea |
|---|---|---|
| `[server]` | `host`, `port` | `127.0.0.1:7777` |
| `[browser]` | `headless`, `max_active`, profile/upload/download/artifact dirs | headless, bounded workers |
| `[browser]` | `max_js_result_bytes`, `max_js_timeout_ms` | JS eval bounds |
| `[storage]` | `journal_path`, `checkpoints_dir`, `authority_path` | under `./data/storage` |
| `[interface]` | `max_request_bytes`, `max_event_batch`, `max_event_retention` | 1 MiB / 256 / 16k |
| `[interface]` | `max_principals`, `max_in_flight_per_principal` | capacity / fairness |
| `[http]` | outbound allowlists and body/timeout caps | egress policy |

## Bootstrap env (not in config.toml)

Credentials are never stored in `config.toml`. Resolve via:

1. `AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN` /
   `…_PRINCIPAL` / `…_CAPABILITIES` / `…_EXPIRES_AT`
2. Secret file (`BOBBY_BROWSER_BOOTSTRAP_ENV` or OS config
   `…/bobby-browser/bootstrap.env` from `bobby init`)
3. Loopback auto-init on `bobby serve`

See [Authentication](auth.md) and [Run the server](run.md).
