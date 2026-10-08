---
documentedVersion: {{PRODUCT_VERSION}}
---

# JavaScript evaluation

Run a JavaScript expression in a page and get the result back. Evaluation is off by default. Two checks must both pass:

1. The caller holds `javascript:evaluate`.
2. The session was created with `executionPolicy.javascriptEvaluation = true`.

Otherwise the call fails with `policyDenied`.

## Evaluate

Create a session with the policy on:

```json
{"name": "workflow_start", "arguments": {"profile": "default", "url": "https://example.com", "executionPolicy": {"javascriptEvaluation": true}}}
```

Then call the tool:

```json
{"name": "evaluate_javascript", "arguments": {"workflowHandle": "wf_0123456789abcdef0123456789abcdef", "expression": "document.title", "timeoutMs": 5000}}
```

Set `awaitPromise: true` to wait for a returned promise. Over HTTP, submit a primitive command with `kind: "evaluateJavaScript"` and the same `input`.

## Limits

| Key | Default | Effect |
|---|---|---|
| `[browser].max_js_result_bytes` | `65536` | Results are truncated at this size and marked |
| `[browser].max_js_timeout_ms` | `30000` | Caller `timeoutMs` is clamped to this |

The command class is Reconciliable. Evaluation is not available on the Firefox companion engine; use Chromium for it.
