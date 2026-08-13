---
documentedVersion: 0.9.0
---

# Capabilities

Tokens bind one principal to an explicit capability set and expiry. Revocation and
expiry are checked again at dispatch, including long-lived MCP and CDP connections.

Wire strings (camelCase JSON uses these exact values):

| Capability | Wire |
|---|---|
| Session read / write | `session:read` / `session:write` |
| Page read / write | `page:read` / `page:write` |
| Browser mutate | `browser:mutate` |
| File upload / download | `file:upload` / `file:download` |
| JavaScript evaluate | `javascript:evaluate` |
| Intent execute | `intent:execute` |
| Vision assist | `vision:assist` |
| Artifact read / capture | `artifact:read` / `artifact:capture` |
| Context read | `context:read` |
| Recovery read / write | `recovery:read` / `recovery:write` |
| Job submit / read / cancel | `job:submit` / `job:read` / `job:cancel` |
| Authority admin | `authority:admin` |
| Browser fingerprint | `browser:fingerprint` |
| Browser humanize | `browser:humanize` |

## Operation → capability matrix

From `InterfaceOperation::required` (HTTP broker and MCP operations map to these):

| Operation | HTTP / MCP | Required capability |
|---|---|---|
| RuntimeInfo | `GET /v1/runtime` / `runtime_info` | `session:read` |
| CreateSession | `POST /v1/sessions` / `session_create` | `session:write` |
| DeleteSession | `DELETE /v1/sessions/{id}` / `session_close` | `session:write` |
| ReadSession | `GET /v1/sessions` / `session_list` | `session:read` |
| OpenPage | `POST /v1/pages` / `page_open` | `page:write` |
| ReadPage | `form_snapshot` | `page:read` |
| SubmitCommand | `POST /v1/commands` / `command_execute` (+ flat MCP browser tools) | `browser:mutate` |
| CreateCheckpoint | `POST /v1/checkpoints` / `checkpoint_save` | `recovery:write` |
| ReadCheckpoint | `GET /v1/recovery/{id}` / `recovery_status` | `recovery:read` |
| RecoverWorkflow | `POST /v1/recovery/{id}` / `workflow_recover` | `recovery:write` |
| SubscribeEvents | `GET /v1/events` / `events_read` | `session:read` |
| ReadArtifact | `GET /v1/artifacts/{id}` | `artifact:read` |
| ReadContext | `GET /v1/context/ask`, `GET /v1/context/site/{key}` / `context_neighbors` | `context:read` |
| IssuePrincipal | `POST /v1/principals` | `authority:admin` |
| RevokePrincipal | `DELETE /v1/principals/{id}` | `authority:admin` |
| SubmitJob | `POST /v1/jobs` | `job:submit` |
| ReadJob | `GET /v1/jobs/{id}` | `job:read` |
| CancelJob | `DELETE /v1/jobs/{id}` | `job:cancel` |

`activatePage` / MCP `page_activate` and `accessibilitySnapshot` / MCP
`a11y_snapshot` are **primitive commands** (via `command_execute` or the flat
MCP tools), not separate `/v1/pages/...` routes. Both still require
`browser:mutate`. See [Accessibility snapshot](../guides/accessibility-snapshot.md).

Some interface operations (`ClosePage`, `CaptureArtifact`) exist in
the type map for authority checks; prefer the documented HTTP/MCP surfaces
above for public clients.

## Privileged primitives (beyond `browser:mutate`)

Submitting a command still requires `browser:mutate`. Nested commands add:

| Command family | Extra capability |
|---|---|
| File upload | `file:upload` |
| File download | `file:download` |
| Evaluate JavaScript | `javascript:evaluate` (+ session `executionPolicy.javascriptEvaluation`) |
| Any intent | `intent:execute` |
| Intent + file fill (`fill` / `completeForm` with `files`) | `intent:execute` and `file:upload` |
| Vision escalation | `vision:assist` (+ session `executionPolicy.visionAssist` + reachable `[vision]` / vision node endpoint) |
| Structured extraction (`extractStructured` / MCP `extract_structured`) | `vision:assist` (+ session `executionPolicy.visionAssist` + reachable `[vision]` / vision node endpoint) |
| Fingerprint spoofing | `browser:fingerprint` at session creation (+ session `executionPolicy.fingerprint`) |
| Humanized input timing | `browser:humanize` at session creation (+ session `executionPolicy.humanize`) |

Missing capability → `missingCapability` (HTTP 403) with `requiredCapability` set when known.
