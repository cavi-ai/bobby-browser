---
documentedVersion: 0.4.0
---

# JavaScript evaluation

Evaluating arbitrary JavaScript is **deny-by-default** and gated twice; both
gates must pass:

1. **Token capability** — bearer holds `javascript:evaluate`
2. **Per-session execution policy** — session created with
   `executionPolicy.javascriptEvaluation = true`

A session created without an explicit grant (the default) rejects JavaScript
with `policyDenied`, even if the token holds the capability. An unknown session
fails closed.

## Call path

Submit a primitive command envelope via HTTP / MCP / SDK (`command` is nested
`{ kind: "primitive", input: { kind: "evaluateJavaScript", input: … } }`).
The TypeScript SDK's published `PrimitiveCommand` union may lag some MCP/Rust
primitive kinds — prefer MCP schema / Rust types when a helper is missing.

```ts
await client.submit(
  {
    schemaVersion: 2,
    commandId: crypto.randomUUID(),
    workflowId: crypto.randomUUID(),
    attemptId: crypto.randomUUID(),
    sessionId: session.id,
    pageId: page.id,
    deadline: new Date(Date.now() + 30_000).toISOString(),
    command: {
      kind: "primitive",
      input: {
        kind: "evaluateJavaScript",
        input: {
          expression: "document.title",
          timeoutMs: 5_000,
          awaitPromise: false,
        },
      },
    },
  },
  { idempotencyKey: crypto.randomUUID() },
);
```

MCP: flat tool `evaluate_javascript` with
`{ sessionId, pageId, expression, timeoutMs?, awaitPromise?, workflowId? }`,
or the same primitive envelope via `command_execute`.

## Bounds

Configured under `[browser]`:

- `max_js_result_bytes` (default 65536) — result truncated/bounded
- `max_js_timeout_ms` (default 30000) — caller `timeoutMs` is clamped

Successful runs may attach `javaScriptResult` evidence. Command class is
`Reconciliable`.
