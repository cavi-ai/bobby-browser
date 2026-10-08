---
documentedVersion: {{PRODUCT_VERSION}}
---

# Python SDK

`bobby-browser` is a typed client for the [HTTP API](http-api.md). It needs Python 3.10 or later and has no third-party dependencies.

```bash
pip install bobby-browser
```

## Connect

```python
import os
from bobby_browser import BrowserRuntimeClient

client = BrowserRuntimeClient(
    "http://127.0.0.1:7777",
    os.environ["AUTOMATION_RUNTIME_TOKEN"],
)
info = client.runtime_info()
```

The first argument is the server origin; a trailing `/v1` is accepted. Get the token with `bobby token`.

The client sends the authorization, interface version, correlation ID and deadline headers on every request. Pass `RequestOptions` to a call to change them:

| Field | Effect |
|---|---|
| `idempotency_key` | Replay-safe retries for mutating calls |
| `correlation_id` | Override the generated UUID |
| `timeout_ms`, `deadline` | Request deadline (default 30 seconds) |

Responses are limited to 64 MiB before parsing. Raise it up to 256 MiB with `max_json_response_bytes`. Artifacts are limited by their declared size, up to 256 MiB, and verified for length, media type and SHA-256 before they are returned.

## Methods

| Method | Route |
|---|---|
| `runtime_info()` | `GET /v1/runtime` |
| `create_session(input, options=None)` | `POST /v1/sessions` |
| `read_session(session_id=None)` | `GET /v1/sessions`. With an ID it filters the list and raises if nothing matches |
| `delete_session(session_id)` | `DELETE /v1/sessions/{id}` |
| `open_page(input, options=None)` | `POST /v1/pages` |
| `read_page(session_id, page_id, max_controls=None)` | `GET /v1/sessions/{session}/pages/{page}/forms` |
| `submit_command(envelope, options=None)` | `POST /v1/commands`. Returns the outcome with its `status` unchanged |
| `create_checkpoint(input, options=None)` | `POST /v1/checkpoints` |
| `recovery_status(workflow_id)` | `GET /v1/recovery/{id}` |
| `recover_workflow(workflow_id)` | `POST /v1/recovery/{id}`. `needsReconciliation` is HTTP 409 |
| `context_ask(session_id, page_id, description)` | `GET /v1/context/ask` |
| `context_site(site_key)` | `GET /v1/context/site/{key}` |
| `read_artifact(reference)` | `GET /v1/artifacts/{id}` |
| `submit_job`, `job_status`, `cancel_job` | `/v1/jobs` |
| `resolve_job(job_id, input)` | `POST /v1/jobs/{job}/resolution` |

`resolve_job` records an operator attestation for a job whose outcome is uncertain. `input` is `{"decision": "effectObserved" | "effectAbsent", "evidenceSha256": ...}`. The caller needs `job:read`, `job:cancel` and `authority:admin`. See [Events and recovery](../guides/events-recovery.md).

## Commands

`submit_command` takes a command envelope as a dictionary. The shape is in the [OpenAPI spec](../openapi/v1.yaml):

```python
import uuid
from datetime import datetime, timedelta, timezone

session = client.create_session({"profile": "default", "proxy": None})
page = client.open_page({"session_id": session["id"]})
outcome = client.submit_command({
    "schemaVersion": 2,
    "commandId": str(uuid.uuid4()),
    "workflowId": str(uuid.uuid4()),
    "attemptId": str(uuid.uuid4()),
    "sessionId": session["id"],
    "pageId": page["id"],
    "deadline": (datetime.now(timezone.utc) + timedelta(seconds=60)).isoformat(),
    "command": {
        "kind": "primitive",
        "input": {"kind": "navigate", "input": {"url": "https://example.com", "waitUntil": "domContentLoaded", "timeoutMs": 30000}},
    },
})
client.delete_session(session["id"])
```

The SDK has no intent helpers. Send intent envelopes the same way (see [Intent commands](../guides/intents.md)), or use the MCP `intent_*` tools.

## Errors

Failures raise `bobby_browser.RuntimeClientError` with `.kind` set to `http`, `transport`, `deadline`, `aborted` or `protocol`. HTTP errors also expose `.status`, `.code`, `.retryable`, `.retry_after_ms`, `.reconciliation_required` and `.required_capability`. The token never appears in error messages.

## Hermes skill

`bobby install --skill-hermes` installs a skill that drives this client for Hermes agents.

## Next

- [First session from code](../introduction/first-session.md)
- [HTTP API](http-api.md)
- [TypeScript SDK](typescript-sdk.md)
