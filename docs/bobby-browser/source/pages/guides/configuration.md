---
documentedVersion: {{PRODUCT_VERSION}}
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

Engine choice is **not** a TOML field. Every entry point — `bobby serve`, the
stdio MCP gateway, and `bobby doctor` — resolves the browser selection through
one canonical order:

1. `AUTOMATION_RUNTIME_BROWSER_SELECTION` (JSON) — an override; wins when set.
2. The persisted enrollment at
   `<config-dir>/bobby-browser/browser-selection.json`, written atomically
   (owner-only, `0600`, on Unix) by `bobby enroll-firefox-profile`.
3. The built-in default: exact **Firefox** (fail-closed — with no enrolled
   profile, startup fails with an actionable error rather than silently
   downgrading engines).

A source that is present but malformed is always an error, never skipped.
`bobby doctor` reports which source resolved.

## `[storage]`

| Field | Default | Meaning |
|---|---|---|
| `journal_path` | `./data/storage/commands.jsonl` | Append-only command journal |
| `scheduler_journal_path` | `./data/storage/scheduler-jobs.jsonl` | Append-only job scheduler journal |
| `checkpoints_dir` | `./data/storage/checkpoints` | Journal checkpoints |
| `authority_path` | `./data/storage/authority.json` | Authority storage |

## `[context]`

Durable shared context graph (remembered form structure per site). Only
runtimes whose engine selection carries a durable profile identity (Firefox
companion enrollment) open the store; Chromium sessions read and write
nothing.

| Field | Default | Meaning |
|---|---|---|
| `dir` | `<config-dir>/bobby-browser/context` (filled by `bobby serve`) | Store root; the profile id is appended as a subdirectory. Unset disables promotion |
| `ttl_days` | `90` | Days a control record is kept without a verified success; swept at store open |

## `[http]` (outbound)

Controls egress from the runtime (downloads, fetches), not the broker listen
socket: `allow_loopback`, `allow_private_network`, redirect/body/timeout caps,
`max_concurrent_requests`. Defaults deny private/loopback egress.

## `[vision]`

Vision can use either a direct HTTP provider or an ACP harness. ACP is the
recommended path when Codex, Claude, OpenCode, Hermes, OpenClaw, or another
workflow harness already owns the model login: Bobby never receives or stores
that provider token.

```bash
bobby vision connect --yes --backend acp --provider codex \
  --command codex --arg acp --auth advertised
```

```toml
[vision]
backend = "acp"
profile = "codex"

[vision.acp_profiles.codex]
command = "codex"
args = ["acp"]
auth = "advertised"
```

Supported auth paths are `advertised`, `oauth-authorization-code`,
`oauth-device-code`, `environment`, `existing-session`, and `none`. For the
three OAuth/advertised modes, Bobby invokes the authentication method the ACP
harness advertised; the harness conducts the login and retains credentials.
Each vision task gets a new ACP child session, bounded text and image content,
a strict JSON result, evidence-digest validation, and an explicit close.

Deny-by-default direct HTTP vision-assist provider. Unset `endpoint_url` means
escalation is unavailable even when the bearer and session opt in.

| Field | Default | Meaning |
|---|---|---|
| `endpoint_url` | unset | Bobby → proxy URL — **https**, or **http only on loopback** |
| `token_env` | unset | Env var name holding the loopback bearer (never store the token here) |
| `timeout_ms` | `15000` | Per-proposal HTTP timeout |
| `provider` | unset | Active profile name under `[vision.providers]` |
| `providers.<name>` | unset | Named OpenAI-compatible upstream profiles |

Each `[vision.providers.<name>]` profile:

| Field | Required | Meaning |
|---|---|---|
| `base_url` | yes | Upstream OpenAI-compatible API base (proxy → provider) |
| `model` | yes | Model id passed to the upstream |
| `api_key_env` | no | Env var for the upstream API key; omit for local servers (Ollama, LM Studio) |

Request / response shapes and confidence floor: [Intent commands](intents.md#vision-provider).
Capability + session gates: [Capabilities](../concepts/capabilities.md).

Granting `vision:assist` and creating a session with
`executionPolicy.visionAssist = true` is **not** enough for functional vision
assist — the runtime must also reach a live provider at `[vision].endpoint_url`.
When the URL is unset, escalation is unavailable even with capability and
session opt-in. When the URL is set but nothing is listening,
`bobby doctor` warns on `vision-endpoint` reachability (loopback endpoints
suggest `bobby serve --vision` or manual `bobby vision-proxy`; external
endpoints suggest verifying the remote service).

Code-review-graph answers **code structure**; bobby vision answers **page
pixels** — do not conflate the two.

### Setup (preferred)

1. Run `bobby vision connect` (interactive menu or `--yes --provider …`) to
   write `endpoint_url`, `token_env`, `provider`, and the matching
   `[vision.providers.*]` table.
2. Export env vars the connect step printed (`BOBBY_VISION_TOKEN`, and
   `api_key_env` when the profile requires one).
3. Start `bobby serve --vision` — on loopback, bobby auto-spawns
   `bobby vision-proxy` when the port is free.

Manual `bobby vision-proxy` in a separate terminal remains valid when you want
full control over the sidecar process.

```bash
export BOBBY_VISION_TOKEN=…
export OPENAI_API_KEY=…       # openai profile only
bobby vision connect --yes --provider openai
bobby serve --vision
```

### Preset providers

| Provider | `base_url` (default) | `model` (default) | `api_key_env` |
|---|---|---|---|
| `openai` | `https://api.openai.com/v1` | `gpt-4o-mini` | `OPENAI_API_KEY` |
| `ollama` | `http://127.0.0.1:11434/v1` | `llava` | — |
| `lmstudio` | `http://127.0.0.1:1234/v1` | `local-model` | — |

For LM Studio (or MLX-hosted OpenAI-compatible servers), copy the **Server
URL** the app displays — port **1234** is the common default, not a guarantee.
Override with `bobby vision connect --base-url …` or edit
`[vision.providers.<name>].base_url` after connect.

### Custom provider

Any OpenAI-compatible endpoint:

```bash
bobby vision connect --yes --provider custom \
  --base-url https://my-host/v1 \
  --model my-vision-model \
  --api-key-env MY_VISION_API_KEY
export MY_VISION_API_KEY=…
export BOBBY_VISION_TOKEN=…
bobby serve --vision
```

Or hand-edit:

```toml
[vision]
endpoint_url = "http://127.0.0.1:9100/vision"
token_env = "BOBBY_VISION_TOKEN"
provider = "myhost"

[vision.providers.myhost]
base_url = "https://my-host/v1"
model = "my-vision-model"
api_key_env = "MY_VISION_API_KEY"   # omit when the upstream needs no key
```

`bobby doctor` warns on `vision-provider` when `provider` names a missing
profile, and on `vision-upstream-key` when a profile's `api_key_env` is unset.

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
