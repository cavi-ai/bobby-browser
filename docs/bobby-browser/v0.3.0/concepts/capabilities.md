---
documentedVersion: 0.3.0
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
| Recovery read / write | `recovery:read` / `recovery:write` |
| Authority admin | `authority:admin` |

## Operation → capability matrix

From `InterfaceOperation::required` (HTTP broker and MCP operations map to these):

| Operation | HTTP / MCP | Required capability |
|---|---|---|
| RuntimeInfo | `GET /v1/runtime` / `runtime_info` | `session:read` |
| CreateSession | `POST /v1/sessions` / `session_create` | `session:write` |
| ReadSession | `GET /v1/sessions` / `session_list` | `session:read` |
| OpenPage | `POST /v1/pages` / `page_open` | `page:write` |
| SubmitCommand | `POST /v1/commands` / `command_execute` | `browser:mutate` |
| CreateCheckpoint | `POST /v1/checkpoints` / `checkpoint_save` | `recovery:write` |
| RecoverWorkflow | `POST /v1/recovery/{id}` / `workflow_recover` | `recovery:write` |
| SubscribeEvents | `GET /v1/events` / `events_read` | `session:read` |
| ReadArtifact | `GET /v1/artifacts/{id}` | `artifact:read` |
| IssuePrincipal | `POST /v1/principals` | `authority:admin` |
| RevokePrincipal | `DELETE /v1/principals/{id}` | `authority:admin` |

Some interface operations (`DeleteSession`, `ReadPage`, `ClosePage`, `ReadCheckpoint`,
`CaptureArtifact`) exist in the type map for authority checks but are not separate
public HTTP routes in the current broker.

## Privileged primitives (beyond `browser:mutate`)

Submitting a command still requires `browser:mutate`. Nested commands add:

| Command family | Extra capability |
|---|---|
| File upload | `file:upload` |
| File download | `file:download` |
| Evaluate JavaScript | `javascript:evaluate` (+ session `executionPolicy.javascriptEvaluation`) |
| Any intent | `intent:execute` |
| Intent + file fill | `intent:execute` and `file:upload` |
| Vision escalation | `vision:assist` (+ session `executionPolicy.visionAssist`) |

Missing capability → `missingCapability` (HTTP 403) with `requiredCapability` set when known.
