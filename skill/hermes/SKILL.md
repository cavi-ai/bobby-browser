---
name: bobby-browser
description: >
  Drive the bobby-browser automation runtime from Hermes with the Python SDK
  (`bobby_browser.BrowserRuntimeClient`) over the authenticated HTTP `/v1`
  surface. Use whenever the task involves browsing a page, filling or
  submitting a form, extracting page data, taking screenshots, reading cookies
  or network logs, hitting a captcha or verification widget, or checkpointing
  and recovering a browser workflow.
---

bobby-browser is a browser automation runtime, not an agent. Drive it with
`BrowserRuntimeClient`. Never claim an action worked unless the returned
`CommandOutcome.status` is `completed`.

## Install the client

```bash
pip install bobby-browser
```

From a bobby-browser checkout: `pip install -e packages/python-sdk`.

The runtime must already be up (`bobby serve`). The bearer is the plaintext
from `bobby token` / `bobby init`, exported as `AUTOMATION_RUNTIME_TOKEN`.
Do not log it. `BrowserRuntimeClient.__repr__` redacts it.

## Client

```python
import os
from bobby_browser import BrowserRuntimeClient, RequestOptions, RuntimeClientError

client = BrowserRuntimeClient(
    "http://127.0.0.1:7777",
    os.environ["AUTOMATION_RUNTIME_TOKEN"],
)
```

`base_url` is the broker origin without a trailing `/v1` (a trailing `/v1` is
stripped). Default loopback port is `7777`.

## Loop

1. `runtime_info()` to confirm the runtime is the one you expect.
2. `create_session({"profile": "default", "proxy": None})` then `open_page(...)`.
   Session id is `session["id"]`.
3. Mutate with `submit_command(envelope)` (`POST /v1/commands`). Read
   `outcome["status"]` before the next mutating call. Any status other than
   `completed` is a failure (`retryableFailure`, `needsReconciliation`,
   `policyDenied`, `resourceExhausted`, `failed`). Repair from the outcome
   error; do not guess the next click.
4. On a site this runtime has seen, `context_ask(session_id, page_id, description)`
   before a snapshot. A remembered answer beats a live read.
5. Recovery: `recovery_status(workflow_id)` then `recover_workflow(workflow_id)`.
   `needsReconciliation` is HTTP 409.

Pass `RequestOptions(idempotency_key=...)` on mutating calls that may retry.

This SDK ships primitives only: there are no `intent_*` helpers. Build a raw
`CommandEnvelope` (see `docs/bobby-browser/source/openapi/v1.yaml` /
`command.schema.json`) and pass it to `submit_command`. For intent-level
workflows, use MCP `intent_*` tools or the TypeScript/Rust SDKs.

Full method catalog: `docs/bobby-browser/source/pages/surfaces/python-sdk.md`.
