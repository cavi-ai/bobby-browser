---
documentedVersion: {{PRODUCT_VERSION}}
---

# Accessibility snapshot

An accessibility snapshot is a compact tree of what is on the page: roles, names, form state, and a ready-to-use `target` for every actionable element. Agents read it to decide what to do, then pass a `target` to `click`, `type_text`, `upload_files` or an intent. It needs `browser:mutate` and does not change the page.

## Take a snapshot

MCP:

```json
{"name": "a11y_snapshot", "arguments": {"workflowHandle": "wf_0123456789abcdef0123456789abcdef", "maxNodes": 256}}
```

HTTP: submit a primitive command with `kind: "accessibilitySnapshot"` and `input: {"maxNodes": 256}` to `POST /v1/commands`.

`maxNodes` is 1 to 2048 and defaults to 256. When the page has more nodes, the evidence sets `truncated: true`; raise `maxNodes` or pass `target` to scope the snapshot to one region, such as the form you are working on. Over MCP, `workflow_observe` returns the same tree with retained context.

## Evidence

```ts
{ kind: "accessibilitySnapshot", pageId: string, nodes: AccessibilityNode[], truncated: boolean }
```

Each node is `{role, name, children?}`. Form controls add optional state:

| Field | Meaning |
|---|---|
| `value` | Current value. Sensitive values read `"[redacted]"` |
| `description` | Accessible description |
| `required`, `disabled`, `readOnly`, `invalid` | Constraint state |
| `checked` | Checkbox or radio state |
| `autocomplete` | Autocomplete token |
| `valueMin`, `valueMax` | Range bounds |
| `target` | `{role, accessibleName, ordinal?}` for actionable nodes |

Repeated role and name pairs get zero-based `ordinal` values in tree order. Ordinals count every matching control on the page, even ones cut by `maxNodes`. A node whose name is redacted has no `target`.

## Use a target

Pass it unchanged:

```json
{"name": "type_text", "arguments": {
  "workflowHandle": "wf_0123456789abcdef0123456789abcdef",
  "target": {"role": "textbox", "accessibleName": "Phone", "ordinal": 1},
  "value": "555-0100"
}}
```

Over HTTP, a primitive command still has a `selector` field. Send `selector: ""` next to `target`.

For intents, convert the target to hints. In TypeScript:

```ts
import { fillEnvelope, intentHintsFromAccessibilityTarget } from "@cavi-ai/bobby-browser";

fillEnvelope(meta, "enter phone", { kind: "setText", value: "555-0100" },
  intentHintsFromAccessibilityTarget(node.target!));
```

A snapshot is not proof that an action worked. Check the command or intent evidence.

## Next

- [MCP tools](../surfaces/mcp-tools.md)
- [Intent commands](intents.md)
