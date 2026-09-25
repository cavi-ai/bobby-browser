---
documentedVersion: {{PRODUCT_VERSION}}
---

# Python SDK

Package: `bobby-browser` (Python >= 3.10, stdlib only -- no third-party HTTP
client dependency).

## Install

```bash
pip install bobby-browser
```

From a bobby-browser checkout: `pip install -e packages/python-sdk`.

`bobby install --skill-hermes` installs the Hermes skill that drives this
client into `$HERMES_HOME/skills/` (else `~/.hermes/skills/`).

## Construct the client

```python
import os
from bobby_browser import BrowserRuntimeClient

client = BrowserRuntimeClient(
    "http://127.0.0.1:7777",
    os.environ["AUTOMATION_RUNTIME_TOKEN"],
)
```

`base_url` should be the broker origin without a trailing `/v1` (the client
strips a trailing `/v1` if present). The bearer is the plaintext from
`bobby init` / bootstrap (conventional env name `AUTOMATION_RUNTIME_TOKEN`).

## Headers

Every request sends:

- `Authorization: Bearer …`
- `x-interface-version: {{INTERFACE_VERSION}}` (`bobby_browser.INTERFACE_VERSION`)
- `x-correlation-id` (UUID4; override via `RequestOptions(correlation_id=...)`)
- `x-deadline` (from `RequestOptions(deadline=...)` / `timeout_ms`, default 30s)

Pass `RequestOptions(idempotency_key=...)` on mutating calls for replay-safe
retries.

## Method catalog

| Method | HTTP | Notes |
|---|---|---|
| `runtime_info()` | `GET /v1/runtime` | |
| `create_session(input, options=None)` | `POST /v1/sessions` | |
| `read_session(session_id=None, options=None)` | `GET /v1/sessions` | No single-id `GET` exists on the wire; with `session_id` this filters the list client-side and raises if no active session matches, with it omitted this returns the full list |
| `delete_session(session_id, options=None)` | `DELETE /v1/sessions/{id}` | 204 on success |
| `open_page(input, options=None)` | `POST /v1/pages` | |
| `read_page(session_id, page_id, max_controls=None, options=None)` | `GET /v1/sessions/{session}/pages/{page}/forms` | Read-only, validated `FormSnapshot`; the PageRead HTTP surface, same contract as MCP `form_snapshot` |
| `submit_command(envelope, options=None)` | `POST /v1/commands` | Raw `CommandEnvelope` in, `CommandOutcome` out with its `status` discriminator preserved exactly |
| `create_checkpoint(input, options=None)` | `POST /v1/checkpoints` | |
| `recovery_status(workflow_id, options=None)` | `GET /v1/recovery/{id}` | |
| `recover_workflow(workflow_id, options=None)` | `POST /v1/recovery/{id}` | `needsReconciliation` maps to HTTP 409; every other decision to 200 |
| `context_ask(session_id, page_id, description, options=None)` | `GET /v1/context/ask` | |
| `context_site(site_key, options=None)` | `GET /v1/context/site/{key}` | |
| `read_artifact(reference, options=None)` | `GET /v1/artifacts/{id}` | Verified bytes: content type, content length, and SHA-256 are checked before anything is returned |
| `submit_job(input, options=None)` | `POST /v1/jobs` | |
| `job_status(job_id, options=None)` | `GET /v1/jobs/{id}` | |
| `cancel_job(job_id, options=None)` | `DELETE /v1/jobs/{id}` | 204 on success |

There is no principals helper on the client today — mint/revoke with raw
HTTP (see [Authentication](../guides/auth.md)).

## Intents

This first Python SDK ships primitives only: build a raw `CommandEnvelope`
dict (matching `docs/bobby-browser/source/openapi/v1.yaml` /
`command.schema.json`) and pass it to `submit_command()`. There are no
`locate` / `fill` / `follow` / `complete_form` intent-envelope helpers yet
(compare the TypeScript SDK's `intents.ts`) — use the MCP `intent_*` tools or
the TypeScript/Rust SDKs for intent-level workflows in the meantime.

## Errors

Failures raise `bobby_browser.RuntimeClientError` with `.kind` of `"http"` |
`"transport"` | `"deadline"` | `"aborted"` | `"protocol"`. HTTP interface
errors expose `.status`, `.code` (wire `InterfaceErrorCode`), `.retryable`,
`.retry_after_ms`, `.reconciliation_required`, and `.required_capability`.
The bearer token is never included in any error message or `repr()`.

## Next

- [First browser session](../introduction/first-session.md)
- [HTTP API reference](http-api.md)
- [Authentication](../guides/auth.md)
- [TypeScript SDK](typescript-sdk.md)
