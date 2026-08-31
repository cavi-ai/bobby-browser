---
documentedVersion: 0.12.0
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
selectors, bounds, raw HTML, or browser backend IDs. Form controls may also
carry structured state (all optional; omitted when unknown):

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
| `target` | `{ role, accessibleName, ordinal? }` | Command-ready semantic target |

Sensitive values (password controls, masked AX values, companion secret
heuristics) serialize as `"[redacted]"` rather than the live contents.

Actionable nodes with a non-empty accessible name include a command-ready
`target`. Unique role/name pairs omit `ordinal`. Repeated pairs receive stable,
zero-based ordinals in accessibility-tree order (the second `Phone` textbox has
`ordinal: 1`). Duplicate accounting covers the full engine snapshot before
`maxNodes` truncation, so a retained target keeps its ordinal even when another
matching control is outside the returned tree. Targets are omitted when the
name is redacted.

### Using `target` in commands

**MCP `click` / `type_text` / `upload_files`** — pass the snapshot `target`
with `sessionId` and `pageId`; omit `selector`. `upload_files` also requires
`paths`. A selector is required only on the legacy raw-selector path. See
[MCP tools](../surfaces/mcp-tools.md).

**HTTP / TypeScript primitives** — `ClickCommand`, `TypeTextCommand`, and
`UploadFilesCommand` still carry a required `selector: string` on the wire.
When driving from a snapshot `target`, set `selector: ""` and pass `target`
(the SDK accepts a minimal `{ role, accessibleName, ordinal? }` as
`TargetSpec`). Prefer MCP flat tools when you want to omit `selector`
entirely.

**Intent targeting** — convert the snapshot target into intent hints. The SDK
helper preserves `role`, accessible name, and `ordinal`, so the same flow works
for both unique and duplicate controls:

```ts
import { fillEnvelope, intentHintsFromAccessibilityTarget } from "@cavi-ai/bobby-browser";

const node = /* AccessibilityNode with target */;
await client.submit(
  fillEnvelope(
    meta,
    "enter phone",
    { kind: "text", text: "555-0100", clearFirst: true },
    intentHintsFromAccessibilityTarget(node.target!),
  ),
  { idempotencyKey: crypto.randomUUID() },
);
```

For primitive commands, `TargetSpec` fields are optional in the TypeScript SDK,
matching the wire schema. Copy the snapshot target and pair it with an empty
selector string on HTTP/TS:

```ts
await client.submit(/* envelope with */ {
  kind: "click",
  input: { selector: "", target: node.target!, boundary: false },
});
```

Always verify with command / intent evidence — do not treat the snapshot alone
as postcondition proof.

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
