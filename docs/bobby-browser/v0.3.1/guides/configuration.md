---
documentedVersion: 0.3.1
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
| `scheduler_journal_path` | `./data/storage/scheduler-jobs.jsonl` | Append-only job scheduler journal |
| `checkpoints_dir` | `./data/storage/checkpoints` | Journal checkpoints |
| `authority_path` | `./data/storage/authority.json` | Authority storage |

## `[http]` (outbound)

Controls egress from the runtime (downloads, fetches), not the broker listen
socket: `allow_loopback`, `allow_private_network`, redirect/body/timeout caps,
`max_concurrent_requests`. Defaults deny private/loopback egress.

## `[vision]`

Deny-by-default HTTP vision-assist provider. Unset `endpoint_url` means
escalation is unavailable even when the bearer and session opt in.

| Field | Default | Meaning |
|---|---|---|
| `endpoint_url` | unset | Provider URL — **https**, or **http only on loopback** |
| `token_env` | unset | Env var name holding the provider bearer (never store the token here) |
| `timeout_ms` | `15000` | Per-proposal HTTP timeout |

Request / response shapes and confidence floor: [Intent commands](intents.md#vision-provider).
Capability + session gates: [Capabilities](../concepts/capabilities.md).

`[vision]` is the single-provider form. `[nodes]` supersedes it — see below.

## `[nodes.<name>]`

Named, separately addressable nodes. A session picks one by name through
`executionPolicy.visionNode`; nothing is process-wide.

| Field | Default | Meaning |
|---|---|---|
| `kind` | required | `vision` — proposes an action from a screenshot. The only kind today; an unknown kind fails config load rather than being ignored. |
| `endpoint_url` | required | Node URL — **https**, or **http only on loopback** |
| `token_env` | unset | Env var name holding the node bearer (never store the token here) |
| `timeout_ms` | `15000` | Per-call HTTP timeout |

```toml
[nodes.local-vision]
kind = "vision"
endpoint_url = "http://127.0.0.1:8080/propose"
```

A session that names no node escalates to no node. A session that names a node
which is not configured is declined: the runtime never substitutes a different
node, and never falls back to a remote default.

Retained page context — what `context_ask` answers from — is held in-process,
not in a node. There is deliberately no `kind = "context"`: an operator could
write it and it would reach nothing.

Locality comes from the address, not from a setting. A session bound to a
loopback node cannot have its screenshots or page text leave the machine.

When both `[vision]` and `[nodes]` are present, `[nodes]` wins and `[vision]`
is ignored with a startup warning. With only `[vision]` set, that endpoint is
reachable as a node named `vision`.

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
| `max_rejection_workers` | `16` | Concurrent rejection / policy-worker permits (must be > 0) |

## Bootstrap env (not in config.toml)

Credentials are never stored in `config.toml`. Resolve via:

1. `AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN` /
   `…_PRINCIPAL` / `…_CAPABILITIES` / `…_EXPIRES_AT`
2. Secret file (`--bootstrap-env` / `BOBBY_BROWSER_BOOTSTRAP_ENV` or OS config
   `…/bobby-browser/bootstrap.env` from `bobby init`)
3. Loopback auto-init on `bobby serve`

See [Authentication](auth.md), [CLI reference](cli.md), and [Run the server](run.md).
