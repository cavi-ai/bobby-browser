---
documentedVersion: {{PRODUCT_VERSION}}
---

# Configuration

bobby reads `config.toml` from the scope's config directory (the OS config directory under `bobby-browser/` for the personal scope). `--config` or `BOBBY_BROWSER_CONFIG` selects another file. The working directory never selects the config, so agents started anywhere reach the same runtime. Relative paths in the file resolve next to it. A missing file uses the defaults below. A malformed file stops startup and names the path.

Credentials never go in `config.toml`. See [Authentication](auth.md).

## `[server]`

| Key | Default | Meaning |
|---|---|---|
| `host` | `127.0.0.1` | Bind address. Keep loopback unless you control the network |
| `port` | `7777` | HTTP port for `/healthz` and `/v1/*` |
| `shutdown_timeout_ms` | `10000` | Grace period on shutdown |

## `[browser]`

| Key | Default | Meaning |
|---|---|---|
| `executable` | from `BOBBY_CHROME_EXECUTABLE` | Chromium binary |
| `profiles_dir` | `./data/profiles` | Per-profile browser state |
| `headless` | `true` | Run without a window |
| `max_active` | `8` | Concurrent browser workers |
| `upload_roots` | `["./data/uploads"]` | Directories uploads may read from |
| `downloads_dir` | `./data/downloads` | Download destination |
| `artifacts_dir` | `./data/artifacts` | Screenshots and other artifacts |
| `max_artifact_bytes` | `8388608` | Largest single artifact |
| `max_screenshot_dimension` | `16384` | Largest screenshot width or height |
| `max_js_result_bytes` | `65536` | JavaScript result bound |
| `max_js_timeout_ms` | `30000` | Ceiling for evaluation `timeoutMs` |

### Engine selection

The engine is not a TOML key. The runtime resolves it in this order:

1. `AUTOMATION_RUNTIME_BROWSER_SELECTION` (JSON), when set.
2. The paired profile in `browser-selection.json`, written by Firefox pairing.
3. Firefox. With no paired profile, startup fails with an actionable error.

A selection that is present but malformed is an error. `bobby doctor` reports which source won.

A managed Chromium selection can use a durable profile with `{"mode": "exact", "engine": "chromium", "profileId": "<name>"}`. Its data lives in `<profiles_dir>/chromium/<name>`. Without `profileId`, each session gets a disposable profile.

## `[storage]`

| Key | Default | Meaning |
|---|---|---|
| `journal_path` | `./data/storage/commands.jsonl` | Command journal |
| `scheduler_journal_path` | `./data/storage/scheduler-jobs.jsonl` | Job journal |
| `checkpoints_dir` | `./data/storage/checkpoints` | Checkpoints |
| `authority_path` | `./data/storage/authority.json` | Authority records |

## `[context]`

Remembered form structure per site. See [Context graph](../concepts/context-graph.md). It opens for profiles with a durable identity: a paired Firefox profile, a Chromium selection with `profileId`, or managed Chromium (stored under `managed-chromium`).

| Key | Default | Meaning |
|---|---|---|
| `dir` | `context` in the config directory | Store root. Unset disables remembering |
| `ttl_days` | `90` | Days to keep a control without a verified success |

## `[mcp]`

| Key | Default | Meaning |
|---|---|---|
| `startup_toolset` | unset (`explore`) | Toolset a connection starts with: `full`, `explore`, `act`, `intent`, `verify`. `BOBBY_MCP_TOOLSET` overrides it |

A smaller starting set shrinks the `tools/list` an agent downloads at connect. It only changes what is advertised; hidden tools stay callable. An invalid `startup_toolset` stops startup, while an invalid `BOBBY_MCP_TOOLSET` is ignored with a warning. See [MCP tools](../surfaces/mcp-tools.md#toolsets).

```toml
[mcp]
startup_toolset = "intent"
```

## `[http]`

Limits on outbound requests the runtime makes, such as downloads. These are not the server's own listener.

| Key | Default | Meaning |
|---|---|---|
| `allow_loopback` | `false` | Allow requests to loopback addresses |
| `allow_private_network` | `false` | Allow requests to private networks |
| `max_redirects` | `5` | Redirect limit |
| `max_header_bytes` | `65536` | Response header limit |
| `max_body_bytes` | `8388608` | Response body limit |
| `max_download_bytes` | `67108864` | Largest download. Advertised as the maximum for `download_url.maxBytes` |
| `request_timeout_ms` | `30000` | Per-request timeout |
| `max_concurrent_requests` | `8` | Concurrent outbound requests |

## `[interface]`

| Key | Default | Meaning |
|---|---|---|
| `max_request_bytes` | `1048576` | Largest request body |
| `max_event_batch` | `256` | Events per batch read |
| `max_event_retention` | `16384` | Events kept per principal |
| `max_connections` | `64` | Concurrent connections, including attached MCP and ACP gateways |
| `token_records_path` | `./data/storage/authorities.json` | Issued principal records |
| `max_principals` | `16` | Enrolled principals |
| `max_in_flight_per_principal` | `8` | In-flight HTTP requests per principal. The next gets `resourceExhausted` |
| `max_rejection_workers` | `16` | Concurrent rejection workers (must be above 0) |

## `[cdp]`

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Bind CDP when running `bobby serve` |
| `host` | `127.0.0.1` | Bind address |
| `port` | `9222` | Listen port |
| `auto_session` | `true` | Open a session for a client that has none |

See [Authenticated CDP](../surfaces/cdp.md).

## `[observability]`

| Key | Default | Meaning |
|---|---|---|
| `level` | `info` | Log level |
| `format` | `json` | `json` or `pretty` |
| `sink` | `stdout` | Log destination |

`bobby doctor` can check vision health against objectives under `[observability.slo]`. Both are optional, and an unset one is not evaluated.

| Key | Meaning |
|---|---|
| `vision_max_failure_rate` | Fail when the share of proposals ending `failed` or `timed_out` exceeds this (0.0 to 1.0) |
| `vision_min_acceptance_rate` | Fail when the share of accepted proposals drops below this (0.0 to 1.0) |

## Vision

Vision assist lets an intent ask a model to choose a control from a screenshot. It is off unless the caller holds `vision:assist`, the session sets `executionPolicy.visionAssist`, and a backend is configured here. See [Intent commands](intents.md#vision-assist).

### Set up a provider

```bash
bobby vision connect --yes --provider openai
export BOBBY_VISION_TOKEN=...
export OPENAI_API_KEY=...
bobby serve --vision
```

`connect` writes the settings below. With a loopback `endpoint_url` and a selected `provider`, each runtime starts its own vision proxy on a free loopback port and sends vision requests there. The proxy exits with the runtime. A `bobby vision start` you run yourself serves `endpoint_url` only when no `provider` is selected.

Add `--activate` to readiness-test the provider right away, and `--download-model` to let an MLX setup fetch a missing model. Ollama and LM Studio manage their own servers. `bobby doctor --fix` repeats the readiness test and normalizes bobby-owned settings.

### Direct backend

| Key | Default | Meaning |
|---|---|---|
| `backend` | `direct` | `direct` or `acp` |
| `endpoint_url` | unset | Proxy URL. `https`, or `http` on loopback only. Unset disables escalation |
| `token_env` | unset | Name of the variable holding the bearer |
| `timeout_ms` | `15000` | Per-proposal timeout |
| `provider` | unset | Active profile under `[vision.providers]` |
| `propose_budget_ms` | unset | Round-trip budget. `bobby doctor` warns above it, and `/v1/runtime` reports it as `visionProposeBudgetMs` |
| `health_failure_threshold` | `3` | Consecutive failures before `/v1/runtime` reports the provider `unhealthy` (or `degraded` for budget violations) |
| `prefill` | `true` | Resolve unresolved form fields from one screenshot before filling |

Each `[vision.providers.<name>]` profile has `base_url` (required, OpenAI-compatible), `model` (required) and `api_key_env` (optional; omit for local servers).

```toml
[vision]
endpoint_url = "http://127.0.0.1:9100/vision"
token_env = "BOBBY_VISION_TOKEN"
provider = "myhost"

[vision.providers.myhost]
base_url = "https://vision.example.com/v1"
model = "my-vision-model"
api_key_env = "MY_VISION_API_KEY"
```

Presets for `bobby vision connect --provider`:

| Provider | `base_url` | `model` | `api_key_env` |
|---|---|---|---|
| `openai` | `https://api.openai.com/v1` | `gpt-4o-mini` | `OPENAI_API_KEY` |
| `ollama` | `http://127.0.0.1:11434` | `llava` | none |
| `lmstudio` | `http://127.0.0.1:1234/v1` | `local-model` | none |
| `mlx` | local | selected model | none |
| `custom` | `--base-url` | `--model` | `--api-key-env` |

Override with `--base-url` and `--model`. For LM Studio, use the server URL the app shows.

The proxy reads at most 1 MiB from an upstream response, counted after decompression, and rejects larger replies. Extracted values are capped at 64 KiB. Errors report the provider and HTTP status without the upstream body.

### ACP backend

An ACP harness that already holds the model login (Codex, Claude, OpenCode, Hermes, OpenClaw) can serve vision. bobby never receives that provider token. Each task runs in a fresh child session with bounded text and image content and a strict JSON result.

```bash
bobby vision connect --yes --backend acp --provider codex --command codex --arg acp --auth advertised
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

`auth` is one of `advertised`, `oauth-authorization-code`, `oauth-device-code`, `environment`, `existing-session` or `none`. bobby calls the harness's `authenticate` method that matches the strategy and fails closed if none is advertised. Log in through the harness first. bobby does not read keychains.

### Corpus and training data

| Key | Default | Meaning |
|---|---|---|
| `corpus_dir` | unset | Write privacy-reduced records of vision escalations to `vision-corpus.jsonl` here |
| `collect_training_data` | `false` | Save proxy request and proposal pairs when a masked screenshot is present |
| `training_data_dir` | `vision-training-data` | Destination for `collect_training_data` |

Before capture, bobby covers editable controls, credential-marked elements and inaccessible frames, and strips URLs of credentials, query strings and fragments. Records are skipped if masking fails. Directories use mode 0700 and files 0600 on Unix. Other page content can still appear in screenshots, so keep these directories local and access-controlled.

## `[nodes.<name>]`

Named vision nodes. A session picks one with `executionPolicy.visionNode`.

| Key | Default | Meaning |
|---|---|---|
| `kind` | required | `vision`. Other values fail config load |
| `endpoint_url` | required | `https`, or `http` on loopback |
| `token_env` | unset | Variable holding the bearer |
| `timeout_ms` | `15000` | Per-call timeout |

```toml
[nodes.local-vision]
kind = "vision"
endpoint_url = "http://127.0.0.1:8080/propose"
```

A session that names no node escalates to none. A session that names an unknown node is declined; bobby never substitutes another or falls back to a remote default. A session bound to a loopback node cannot send screenshots or page text off the machine. When `[nodes]` and `[vision]` are both set, `[nodes]` wins with a startup warning. With only `[vision]`, its endpoint is a node named `vision`.
