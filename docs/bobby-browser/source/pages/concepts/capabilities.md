---
documentedVersion: {{PRODUCT_VERSION}}
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

<!-- BEGIN GENERATED INTERFACE SUPPORT -->
## Generated operation support

`direct` means the adapter exposes the operation. `via command` means it is reached through `submitCommand`. The capability column is the direct operation gate; translated paths use `browser:mutate` plus nested command requirements.

| Operation | Required capability | HTTP | MCP | CDP | ACP | Engine scope |
|---|---|---|---|---|---|---|
| `runtimeInfo` | `session:read` | direct | direct | direct | — | engine-agnostic |
| `createSession` | `session:write` | direct | direct | — | direct | Chromium, Firefox |
| `readSession` | `session:read` | direct | direct | direct | — | Chromium, Firefox |
| `deleteSession` | `session:write` | direct | direct | — | direct | Chromium, Firefox |
| `openPage` | `page:write` | direct | direct | direct | direct | Chromium, Firefox |
| `readPage` | `page:read` | direct | direct | via command | via command | Chromium, Firefox |
| `closePage` | `page:write` | via command | via command | via command | via command | Chromium, Firefox |
| `submitCommand` | `browser:mutate` | direct | direct | direct | direct | Chromium, Firefox |
| `createCheckpoint` | `recovery:write` | direct | direct | direct | direct | Chromium, Firefox |
| `readCheckpoint` | `recovery:read` | direct | direct | direct | direct | Chromium, Firefox |
| `recoverWorkflow` | `recovery:write` | direct | direct | — | direct | Chromium, Firefox |
| `readArtifact` | `artifact:read` | direct | — | — | — | engine-agnostic |
| `readContext` | `context:read` | direct | direct | — | direct | Chromium, Firefox |
| `captureArtifact` | `artifact:capture` | via command | via command | direct | — | Chromium, Firefox |
| `subscribeEvents` | `session:read` | direct | direct | direct | — | Chromium, Firefox |
| `submitJob` | `job:submit` | direct | direct | — | — | engine-agnostic |
| `readJob` | `job:read` | direct | direct | — | — | engine-agnostic |
| `cancelJob` | `job:cancel` | direct | direct | — | — | engine-agnostic |
| `issuePrincipal` | `authority:admin` | direct | — | — | — | engine-agnostic |
| `revokePrincipal` | `authority:admin` | direct | — | — | — | engine-agnostic |

## Execution-policy gates

These fields are opt-ins. The capability is checked at the listed operation before protected behavior runs.

| `executionPolicy` field | Capability | Enforced at | HTTP | MCP | CDP | ACP | Engines |
|---|---|---|---|---|---|---|---|
| `javascriptEvaluation` | `javascript:evaluate` | `submitCommand` | yes | yes | yes | — | Chromium, Firefox |
| `visionAssist` | `vision:assist` | `submitCommand` | yes | yes | — | yes | Chromium, Firefox |
| `fingerprint` | `browser:fingerprint` | `createSession` | yes | yes | — | — | Chromium, Firefox |
| `humanize` | `browser:humanize` | `createSession` | yes | yes | — | — | Chromium, Firefox |

<!-- END GENERATED INTERFACE SUPPORT -->

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
