---
documentedVersion: 0.3.0
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

Successful outcomes include `Evidence.AccessibilitySnapshot`:

```ts
{
  kind: "accessibilitySnapshot",
  pageId: string,
  nodes: Array<{ role?: string; name?: string; children?: … }>,
  truncated: boolean,
}
```

Each node is `{ role, name, children }` only — no DOM selectors, bounds, or
raw HTML.

## Engine notes

- **Chromium** — normalizes Chrome's full accessibility tree; ignored nodes
  are skipped and children re-parented; unnamed generic containers are kept
  only for structure.
- **Firefox** — companion extension DOM walker; hidden-aware; values that look
  like secrets (for example password / secret field contents) are redacted
  from names.

## Next

- [MCP tools](../surfaces/mcp-tools.md)
- [HTTP API](../surfaces/http-api.md)
- [Intent commands](intents.md) (semantic targeting after inspecting the tree)
- [Troubleshooting](troubleshooting.md)
