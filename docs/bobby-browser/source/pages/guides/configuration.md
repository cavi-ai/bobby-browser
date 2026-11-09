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

Every session on a durable profile shares one Chrome. A Chrome already running on the profile with remote debugging is attached to; otherwise one starts and keeps running after the runtime exits. Extensions installed in the profile, such as a password manager, run in it. To use an existing profile directory, such as a signed-in Chrome profile, list it:

```json
{
  "preference": {"mode": "exact", "engine": "chromium", "profileId": "signed-in"},
  "chromium": [{"profileId": "signed-in", "profileDir": "/path/to/chrome-profile"}]
}
```

Start that Chrome with `--remote-debugging-port=0` to share it while it runs.

With `headless = true`, shared-profile attachment requires the browser process's first switch to be `--headless` or `--headless=new`, following an unambiguous executable path. Bobby places that switch first when launching a headless profile, including through a shell launcher. A headed browser, unavailable mode information or an ambiguous legacy/manual command line is refused before a page opens. The existing browser is left running. If no DevTools endpoint answers, Bobby launches Chrome only without ownership markers or after proving the local singleton owner is dead. Live, foreign-host and unverified owners are left untouched. Release the existing browser before starting the same profile headlessly; restarting Bobby alone does not change a shared browser's mode. A headed and headless browser cannot concurrently own the same profile directory.

For headless Chromium work, check `runtime_info.capabilities` for `headless-chromium` before `workflow_start`. It is present only when the running default worker selection enforces headless Chromium for every candidate; headed, unknown or mixed-engine fallbacks do not advertise it. This reflects the running factory, not a later edit to `config.toml`. Completed workflow startup then proves the attachment check passed. The normal browser-selection precedence still applies, including `AUTOMATION_RUNTIME_BROWSER_SELECTION` overrides.

Firefox opens new tabs in the background; activation remains an explicit operation. To attach to an existing Firefox without automatic browser repair, set `"attachmentOnly": true` on its existing `firefox` entry in the selection document. Keep its enrolled profile ID, directory, BiDi URL and native-host descriptor, and reuse its original runtime scope. The default is `false`, which retains managed recovery. Attachment-only mode refuses a companion build mismatch, an occupied external BiDi session or an unavailable endpoint before reloading or restarting Firefox. `bobby doctor` fails while that Firefox is down or started before its companion install, and `bobby doctor --fix` restarts it. Closing the runtime ends its own WebDriver session without closing the browser.

Before starting that Firefox workflow, require `firefox-background-attachment` in the running `runtime_info.capabilities`. Every default candidate must be an attachment-only Firefox factory; managed, unknown and mixed-engine selections do not advertise it. This contract covers attachment and background tab creation. Use context-bound navigation, inspection and owned-page cleanup for background research; the capability does not make explicit activation or pointer/keyboard input focus-free. A changed selection file takes effect only when its runtime owner loads it; connecting another agent reuses the existing owner.

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
| `limits.max_file_bytes` | `2097152` | Largest site file; a larger file is preserved, reported and skipped |
| `limits.max_site_records` | `16384` | Records kept per site file |
| `limits.max_resident_sites` | `256` | Sites held in memory per profile |
| `limits.max_resident_bytes` | `67108864` | Memory held per profile before sites reload from disk |

Every limit must be positive. See [Limits](../concepts/context-graph.md#limits).

## `[mcp]`

| Key | Default | Meaning |
|---|---|---|
| `startup_toolset` | unset (`explore`) | Toolset a connection starts with: `full`, `explore`, `act`, `intent`, `verify`. `BOBBY_MCP_TOOLSET` overrides it |
| `success_screenshots` | `false` | A completed `intent_complete_form` or `intent_submit_and_verify` attaches a viewport screenshot, at most 1280 px on its longest side. A call's `screenshot` argument overrides it |
| `workflow_dirs` | `[]` | Directories of [workflow files](workflows.md), read after `<config dir>/workflows`; relative paths resolve against this file's directory |

A smaller starting set shrinks the `tools/list` an agent downloads at connect. It only changes what is advertised; hidden tools stay callable. An invalid `startup_toolset` stops startup, while an invalid `BOBBY_MCP_TOOLSET` is ignored with a warning. See [MCP tools](../surfaces/mcp-tools.md#toolsets).

```toml
[mcp]
startup_toolset = "intent"
```

## `[[credentials]]`

Each entry lets an agent fill a password field by name without seeing the secret. bobby runs `command` only when a fill names the entry for a password field whose page origin is exactly `origin`, then types what it prints into that field.

| Key | Meaning |
|---|---|
| `name` | What a fill passes: `{kind: "credential", name}` |
| `origin` | Exact origin of the password field's page, such as `https://login.example.com` |
| `command` | Program and arguments that print the secret on stdout; one trailing line break is dropped. At most 16 KiB, within 60 seconds |

```toml
[[credentials]]
name = "work"
origin = "https://login.example.com"
command = ["op", "read", "op://Private/Work login/password"]
```

The secret is never returned, logged or journaled, and the command's stderr is discarded. A fill needs the session's `executionPolicy.passwordManager`.

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

Command logs include a `command` span with `command_id`, `workflow_id`,
`attempt_id`, `session_id`, command `family` and `class`, and `runtime_version`.
`command.finished` records the returned outcome status and `elapsed_ms`. If the
command future is dropped before returning, the status is `interrupted`; use the
durable journal and recovery APIs to reconcile effects before retrying.

Polled command submissions reaching the authenticated runtime also emit
`command.request.finished` in a `command_request` span, including authorization
rejections and idempotency replays. The span includes correlation, command,
workflow, attempt, and session IDs, plus `request_mode` (`submit`, `oneShotVision`,
or `autoCheckpoint`) and whether an idempotency key was present. Keys and bearer
credentials are omitted.

Request `disposition` is `runtimeInvoked`, `replayed`, `rejected`, or `interrupted`.
The event includes `outcome`, a bounded `error_code`, `elapsed_ms`, and
`runtime_invoked`, `replayed`, and `interrupted` flags. `outcome_command_id` links
a replay to the original command even when the retry has a new command ID;
it is `none` if no command outcome returned. Error messages are omitted.

Group request events by workflow ID for request, replay, and rejection summaries.
Count `command.finished` separately for execution attempts: invoking the runtime
may fail during checkpoint preparation before command execution begins. Neither
count proves a browser effect. A dropped waiter is interrupted even if it never
invoked the runtime. Transport authentication and parsing failures that never
reach the authenticated runtime are outside this accounting boundary. Log
filtering and retention can omit events; these are diagnostics, not durable audit
records or permission to retry.

Vision calls emit `vision.provider.started` and `vision.provider.finished` within
the command span. The latter includes `provider_mode`, `elapsed_ms`, and
`proposal_returned`. A returned proposal still requires validation and does not
mean an action succeeded. These events omit command payloads, URLs, page content,
screenshots, and provider error text. Logging filters and operator retention
apply; these diagnostics do not replace durable history or enforce provider health.

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
| `python` | unset | Interpreter with the vision server's packages (`scripts/vision-mlx/requirements.txt`); runs the MLX and text-tier servers. Unset runs `BOBBY_VISION_PYTHON`, else `python3` from `PATH` |
| `model_server` | unset | OpenAI-compatible server that already holds the `mlx` provider's model, such as one `mlx-agent serve` started. The managed MLX server sends each request there instead of loading the model |

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
| `ollama` | `http://127.0.0.1:11434` | `qwen3-vl:8b-instruct` | none |
| `lmstudio` | `http://127.0.0.1:1234/v1` | `local-model` | none |
| `mlx` | local | selected model | none |
| `custom` | `--base-url` | `--model` | `--api-key-env` |

Override with `--base-url` and `--model`. For LM Studio, use the server URL the app shows. An MLX `--model` is a Hugging Face repository id or a local model directory; `--python` names the interpreter with the vision server's packages, and `--activate` starts that server and waits for it to accept connections.

Interactive install and `bobby vision connect` list available models after you choose a provider. Type to search, select the model, or choose **Refresh** or **Back**. Ollama and LM Studio report image support, so known text-only models are excluded. OpenAI-compatible providers that omit image metadata show **image support not reported** beside their models; check the provider's model documentation before choosing one. MLX lists cached image models and offers download choices with confirmation.

Model lists are cached in memory for 60 seconds, separately for each provider, endpoint and credential. Refresh queries the provider again; a selected list entry is checked again before it is saved. Provider requests time out after 5 seconds. If discovery is unavailable, you can enter an exact model ID manually.

For unattended setup, use `--yes --model <id>` with `vision connect`, or `--yes --vision-model <id>` with `install`. An explicit model is retained. With no explicit model or existing selection, Ollama uses the default only if it is available, or the sole installed image model; multiple alternatives require a selection. When Ollama cannot be reached, setup can write the tagged default `qwen3-vl:8b-instruct`; readiness is checked when activation is requested. Ollama reads an untagged model name as `:latest`; `bobby doctor --fix` can repair an untagged selection to an installed tag.

The proxy reads at most 1 MiB from an upstream response, counted after decompression, and rejects larger replies. Extracted values are capped at 64 KiB. Errors report the provider and HTTP status without the upstream body.

### ACP backend

An ACP harness that already holds the model login (Codex, Claude, OpenCode, Hermes, OpenClaw) can serve vision. bobby never receives that provider token. bobby starts the harness on the first task and keeps it running; each task opens its own session with bounded text and image content and a strict JSON result, then closes it. A transport failure or timeout stops the harness, and the next task starts a new one.

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

| Key | Default | Meaning |
|---|---|---|
| `env` | `{}` | Environment for the harness process, over bobby's own; `bobby vision login` uses it too |
| `session_meta` | `{}` | Sent as `_meta` on every `session/new` |

By default a harness loads its user's own instructions, hooks, plugins, skills and MCP servers into every vision session. A vision profile can leave them out. For `claude-agent-acp`:

```toml
[vision.acp_profiles.claude]
command = "claude-agent-acp"
auth = "existing-session"

[vision.acp_profiles.claude.session_meta.claudeCode.options]
settingSources = []
tools = []
mcpServers = {}
strictMcpConfig = true
promptSuggestions = false
systemPrompt = "Answer each request with exactly the JSON object it specifies."
```

For Codex, point `CODEX_HOME` at a directory whose `config.toml` sets only the model and provider, then run `bobby vision login codex` once to sign in there:

```toml
[vision.acp_profiles.codex.env]
CODEX_HOME = "/path/to/vision-codex-home"
```

### Corpus and training data

| Key | Default | Meaning |
|---|---|---|
| `corpus_dir` | unset | Write privacy-reduced records of vision escalations to `vision-corpus.jsonl` here |
| `assist_profiles` | `[]` | Browser profiles whose sessions always run with vision assist while a vision provider is configured |
| `collect_training_data` | `false` | Save proxy request and proposal pairs when a masked screenshot is present |
| `training_data_dir` | `vision-training-data` | Destination for `collect_training_data` |

Before capture, bobby covers editable controls, credential-marked elements and inaccessible frames, and strips URLs of credentials, query strings and fragments. Records are skipped if masking fails. Directories use mode 0700 and files 0600 on Unix. Other page content can still appear in screenshots, so keep these directories local and access-controlled.

### On-screen controls

| Key | Default | Meaning |
|---|---|---|
| `detector_model` | unset | ONNX interactable-control detector: `onnx/model.onnx` from [cavi-ai/bobby-control-detector](https://huggingface.co/cavi-ai/bobby-control-detector), which `bobby vision connect --activate --download-model` downloads and sets on macOS, or one `scripts/perception/eval_detector.py --onnx` exports |

With a detector, the vision window of an intent that clicks also offers up to two on-screen controls that no accessibility candidate names and whose text shares a word with the purpose, such as a button drawn on a canvas. Each detected control is named by the text inside it, read with Apple Vision on macOS, and a picked one is clicked at its center. Detection runs locally on the CPU; the screenshot never leaves the machine for it.

### Text tier

`[vision.text_tier]` sends candidate windows to a text-only vision server without a screenshot; every other request, including every extract, keeps the screenshot and the image provider.

| Key | Default | Meaning |
|---|---|---|
| `base_url` | required | Text-tier vision server; `bobby vision connect` writes `http://127.0.0.1:9102` unless `--text-base-url` is given |
| `model` | required | Model the text tier serves |
| `adapter` | unset | Adapter directory the text model loads |

`bobby vision connect --text-model <id> [--text-adapter <dir>]` writes this table. The runtime's managed proxy and `bobby vision start` run the text tier; `bobby vision-proxy` takes `--text-base-url`, `--text-model` and `--spawn-text-server`.

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

A session that names no node escalates to the primary vision node (`vision`, else the first) when its address is loopback, and to none otherwise. A session that names an unknown node is declined; bobby never substitutes another or falls back to a remote default. A session bound to a loopback node cannot send screenshots or page text off the machine. When `[nodes]` and `[vision]` are both set, `[nodes]` wins with a startup warning. With only `[vision]`, its endpoint is a node named `vision`.
