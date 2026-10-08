---
documentedVersion: {{PRODUCT_VERSION}}
---

# Capabilities

A capability is a permission a token carries. Each token binds one principal to an explicit capability set and an expiry. The runtime checks the capability for every operation, and checks expiry and revocation again at dispatch, including on long-lived MCP and CDP connections. A missing capability returns `missingCapability` (HTTP 403) with `requiredCapability` set when known.

Choose a set when you create a credential (`bobby init --preset`) or issue a principal (`POST /v1/principals`). The tables below show what each operation needs and what each preset allows.

## Capability names

JSON uses these exact strings:

| Capability | Wire |
|---|---|
| Session read / write | `session:read` / `session:write` |
| Page read / write | `page:read` / `page:write` |
| Browser mutate | `browser:mutate` |
| Network egress | `network:egress` |
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
| `resolveJob` | `job:read`, `job:cancel`, `authority:admin` | direct | — | — | — | engine-agnostic |
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

<!-- BEGIN GENERATED PRESET MATRIX -->
## Generated preset matrix

`bobby init --preset <name>` mints the loopback credential with one of these sets. A call that needs a capability the credential lacks fails with `missingCapability`.

| Capability | Lets a principal | `unrestricted` | `agent` | `claude` | `codex` | `openshell` |
|---|---|---|---|---|---|---|
| `session:read` | list sessions, read runtime info, subscribe to events | yes | yes | yes | yes | yes |
| `session:write` | create and delete sessions | yes | yes | yes | yes | yes |
| `page:read` | read page state | yes | yes | yes | yes | yes |
| `page:write` | open and close pages | yes | yes | yes | yes | yes |
| `browser:mutate` | submit commands: navigate, click, type | yes | yes | yes | yes | yes |
| `network:egress` | run the HTTP job handlers (http_probe, http_wait, http_fetch) | yes | yes | yes | yes | — |
| `file:upload` | upload local files into the page | yes | yes | yes | yes | yes |
| `file:download` | download files to disk | yes | yes | yes | yes | yes |
| `javascript:evaluate` | run JavaScript in the page | yes | yes | — | — | — |
| `intent:execute` | run intent commands (locate, fill, submit, follow) | yes | yes | yes | yes | yes |
| `vision:assist` | escalate stuck intents and extraction to a vision model | yes | yes | yes | yes | — |
| `artifact:read` | read stored artifacts | yes | yes | yes | yes | yes |
| `context:read` | read remembered site structure | yes | yes | yes | yes | yes |
| `artifact:capture` | capture screenshots and other artifacts | yes | yes | yes | yes | yes |
| `recovery:read` | read checkpoints and recovery state | yes | yes | yes | yes | yes |
| `recovery:write` | save checkpoints and recover workflows | yes | yes | yes | yes | yes |
| `job:submit` | submit background jobs | yes | yes | yes | yes | — |
| `job:read` | read background jobs | yes | yes | yes | yes | — |
| `job:cancel` | cancel background jobs | yes | yes | yes | yes | — |
| `authority:admin` | mint and revoke principals | yes | — | — | — | — |
| `browser:fingerprint` | spoof the browser fingerprint | yes | yes | — | — | — |
| `browser:humanize` | humanize input timing | yes | yes | — | — | — |

## What each preset cannot do

- `unrestricted`: local operator: every capability, including authority:admin. Nothing is withheld.
- `agent`: no authority:admin; every other capability. Cannot:
  - mint and revoke principals (`authority:admin`: `resolveJob`, `issuePrincipal`, `revokePrincipal`)
- `claude`: the shipped agent skill's workflow: no JavaScript evaluation, fingerprint, humanize, or authority:admin. Cannot:
  - run JavaScript in the page (`javascript:evaluate`: `executionPolicy.javascriptEvaluation`)
  - mint and revoke principals (`authority:admin`: `resolveJob`, `issuePrincipal`, `revokePrincipal`)
  - spoof the browser fingerprint (`browser:fingerprint`: `executionPolicy.fingerprint`)
  - humanize input timing (`browser:humanize`: `executionPolicy.humanize`)
- `codex`: the shipped agent skill's workflow: no JavaScript evaluation, fingerprint, humanize, or authority:admin. Cannot:
  - run JavaScript in the page (`javascript:evaluate`: `executionPolicy.javascriptEvaluation`)
  - mint and revoke principals (`authority:admin`: `resolveJob`, `issuePrincipal`, `revokePrincipal`)
  - spoof the browser fingerprint (`browser:fingerprint`: `executionPolicy.fingerprint`)
  - humanize input timing (`browser:humanize`: `executionPolicy.humanize`)
- `openshell`: sandboxed tenant: browse, intents, files, evidence, and recovery only. Cannot:
  - run the HTTP job handlers (http_probe, http_wait, http_fetch) (`network:egress`: checked by the job or command that uses it)
  - run JavaScript in the page (`javascript:evaluate`: `executionPolicy.javascriptEvaluation`)
  - escalate stuck intents and extraction to a vision model (`vision:assist`: `executionPolicy.visionAssist`)
  - submit background jobs (`job:submit`: `submitJob`)
  - read background jobs (`job:read`: `readJob`, `resolveJob`)
  - cancel background jobs (`job:cancel`: `cancelJob`, `resolveJob`)
  - mint and revoke principals (`authority:admin`: `resolveJob`, `issuePrincipal`, `revokePrincipal`)
  - spoof the browser fingerprint (`browser:fingerprint`: `executionPolicy.fingerprint`)
  - humanize input timing (`browser:humanize`: `executionPolicy.humanize`)
<!-- END GENERATED PRESET MATRIX -->
