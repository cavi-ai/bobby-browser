---
documentedVersion: 0.2.0
---

# JavaScript evaluation

Evaluating arbitrary JavaScript is **deny-by-default** and gated twice; both gates must pass:

1. **Token capability** — the bearer must hold `javascript:evaluate`
2. **Per-session execution policy** — the session must have been created with `executionPolicy.javascriptEvaluation = true`

A session created without an explicit grant (the default) rejects JavaScript with `PolicyDenied`, even if the token holds the capability. An unknown session fails closed.

Execution is bounded: result size (`browser.max_js_result_bytes`) and timeout (`browser.max_js_timeout_ms`). A successful run returns a `javaScriptResult` evidence item.
