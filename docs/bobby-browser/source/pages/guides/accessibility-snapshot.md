---
documentedVersion: {{PRODUCT_VERSION}}
---

# Accessibility snapshot

The `accessibilitySnapshot` primitive returns a compact accessibility tree for
the current page. Class: **Replayable**. Capability: `browser:mutate` (no
extra nested capability).

## Surfaces

| Surface | How |
|---|---|
| HTTP / TypeScript SDK | `POST /v1/commands` primitive `accessibilitySnapshot` |
| MCP | Flat tool `a11y_snapshot` (or the same primitive via `command_execute`) |

## Request

```ts
await client.submit(
  {
    schemaVersion: 2,
    commandId: crypto.randomUUID(),
    workflowId: crypto.randomUUID(),
    attemptId: crypto.randomUUID(),
    sessionId: session.id,
    pageId: page.id,
    deadline: new Date(Date.now() + 60_000).toISOString(),
    command: {
      kind: "primitive",
      input: {
        kind: "accessibilitySnapshot",
        input: { maxNodes: 256 },
      },
    },
  },
  { idempotencyKey: crypto.randomUUID() },
);
```

MCP: `a11y_snapshot` with `{ sessionId, pageId, maxNodes? }`.

`maxNodes` is optional. Engines default to **256** and clamp to **1…2048**.
When the live tree exceeds the budget, evidence sets `truncated: true`.

## Evidence

Successful outcomes include `Evidence.accessibilitySnapshot`:

```ts
{
  kind: "accessibilitySnapshot",
  pageId: string,
  nodes: AccessibilityNode[],
  truncated: boolean,
}
```

### Node shape

Every node is still a compact `{ role, name, children? }` tree — no DOM
selectors, bounds, or raw HTML. Form controls may also carry structured state
(all optional; omitted when unknown):

| Field | Wire | Meaning |
|---|---|---|
| `value` | string | Current control value (redacted when sensitive) |
| `description` | string | Accessible description when present |
| `required` | boolean | Required constraint |
| `disabled` | boolean | Disabled |
| `readOnly` | boolean | Read-only |
| `invalid` | boolean | Currently invalid |
| `checked` | boolean | Checkbox / radio checked state |
| `autocomplete` | string | Autocomplete token |
| `valueMin` / `valueMax` | string | Numeric / range bounds when exposed |

Sensitive values (password controls, masked AX values, companion secret
heuristics) serialize as `"[redacted]"` rather than the live contents.

Use the tree to plan `fill` / `completeForm` targeting (`role` + exact
`nearText` from `name`), then verify with intent evidence — do not treat the
snapshot alone as a postcondition proof.

## Engine notes

- **Chromium** — normalizes Chrome's full accessibility tree; ignored nodes
  are skipped and children re-parented; unnamed generic containers are kept
  only for structure; form properties come from AX attributes.
- **Firefox** — companion extension DOM walker; hidden-aware; form attributes
  and validity-related flags are projected into the same node shape; secret-
  like names/values are redacted.

## Next

- [MCP tools](../surfaces/mcp-tools.md)
- [HTTP API](../surfaces/http-api.md)
- [Intent commands](intents.md) (semantic targeting after inspecting the tree)
- [Troubleshooting](troubleshooting.md)
