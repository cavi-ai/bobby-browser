---
documentedVersion: 0.3.0
---

# Configuration

`bobby serve` loads `./config.toml` at startup, overridable with
`--config` / `BOBBY_BROWSER_CONFIG`. A missing file uses built-in defaults; a
malformed file fails startup and names the path.

The committed
[`config.toml`](https://github.com/cavi-ai/bobby-browser/blob/main/config.toml)
is the canonical field list and mirrors `AppConfig` (`crates/config`). Values
below match `AppConfig::default()` unless you override them.

## `[server]`

| Field | Default | Meaning |
|---|---|---|
| `host` | `127.0.0.1` | Bind host (keep loopback unless you control the network) |
| `port` | `7777` | HTTP listen port (`/healthz`, `/v1/*`) |

## `[browser]`

| Field | Default | Meaning |
|---|---|---|
| `profiles_dir` | `./data/profiles` | Per-profile browser state |
| `headless` | `true` | No visible window |
| `max_active` | `8` | Max concurrent browser workers |
| `upload_roots` | `["./data/uploads"]` | Allowed roots for file upload |
| `downloads_dir` | `./data/downloads` | Download output directory |
| `artifacts_dir` | `./data/artifacts` | Screenshots and other artifacts |
| `max_artifact_bytes` | `8388608` | Max single artifact size |
| `max_screenshot_dimension` | `16384` | Max screenshot width/height |
| `max_js_result_bytes` | `65536` | JS eval result bound |
| `max_js_timeout_ms` | `30000` | Clamp for JS `timeout_ms` |

Engine choice is **not** a TOML field — use
`AUTOMATION_RUNTIME_BROWSER_SELECTION` JSON (default exact **Firefox**).

## `[storage]`

| Field | Default | Meaning |
|---|---|---|
| `journal_path` | `./data/storage/commands.jsonl` | Append-only command journal |
| `checkpoints_dir` | `./data/storage/checkpoints` | Journal checkpoints |
| `authority_path` | `./data/storage/authority.json` | Authority storage |

## `[http]` (outbound)

Controls egress from the runtime (downloads, fetches), not the broker listen
socket: `allow_loopback`, `allow_private_network`, redirect/body/timeout caps,
`max_concurrent_requests`. Defaults deny private/loopback egress.

## `[interface]`

| Field | Default | Meaning |
|---|---|---|
| `max_request_bytes` | `1048576` | Max inbound request body |
| `max_event_batch` | `256` | Max events per batch read |
| `max_event_retention` | `16384` | Retained events per principal stream |
| `max_connections` | `64` | Concurrent interface connections |
| `token_records_path` | `./data/storage/authorities.json` | Issued principal records |
| `max_principals` | `16` | Max enrolled principals |
| `max_in_flight_per_principal` | `8` | Fairness / concurrency cap |

## Bootstrap env (not in config.toml)

Credentials are never stored in `config.toml`. Resolve via:

1. `AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN` /
   `…_PRINCIPAL` / `…_CAPABILITIES` / `…_EXPIRES_AT`
2. Secret file (`--bootstrap-env` / `BOBBY_BROWSER_BOOTSTRAP_ENV` or OS config
   `…/bobby-browser/bootstrap.env` from `bobby init`)
3. Loopback auto-init on `bobby serve`

See [Authentication](auth.md), [CLI reference](cli.md), and [Run the server](run.md).
