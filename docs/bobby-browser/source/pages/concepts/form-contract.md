---
documentedVersion: {{PRODUCT_VERSION}}
---

# Canonical form contract

A `FormSnapshot` describes the forms on a page in an engine-neutral shape, so an agent can plan edits before acting. Read one with the MCP `form_snapshot` tool, `GET /v1/sessions/{session}/pages/{page}/forms`, or the SDKs' `formSnapshot` and `read_page`. It needs `page:read`, not `browser:mutate`.

It reports forms, controls outside forms, labeled groups, constraints, validity, options, the operations each control supports, and a safe semantic target. It never includes CSS selectors, DOM or backend IDs, raw HTML, arbitrary attributes or secret values.

```ts
interface FormSnapshot {
  schemaVersion: 1;
  pageId: string;
  forms: FormDescriptor[];
  unownedControls: FormControl[];
  truncated: boolean;
  pageDerived?: true;
}
```

Every control has an ID, its form and group, a normalized `controlKind`, typed current state, constraints, validity, options and `supportedOperations`. bobby stamps the control's number on its element as `data-bobby-control`, so the ID stays with that control in later snapshots of the page, even after the page inserts controls before it. A control's target holds a role, accessible name, optional ordinal and bounded frame or shadow paths. An unnamed control's target has an empty `accessibleName` and `attributes: {"data-bobby-control": "<number>"}`, which identifies the element. If a safe target cannot be produced, the target is absent.

A snapshot is limited to 64 forms and 512 controls. `truncated: true` means discovery hit a limit. Unknown fields and unsupported schema versions are rejected.

## Passwords

A password control never exposes its value. Its state is:

```json
{ "kind": "redacted", "present": true }
```

`present` says only whether a value exists.

For uploads, pass the current file control's `id` as `upload_files.controlId`. Hidden file inputs are eligible. An unnamed file target carries `attributes: {"type": "file"}` and an empty `accessibleName`; its ordinal is scoped to unnamed files in the same frame and shadow root. Upload capability and configured upload roots still apply.

## Act on controls

Pass a control's target to `control_action` (or the `controlAction` primitive). Before acting, bobby rereads the control and checks that it supports the operation. It then performs the operation once and returns evidence with the operation, target, typed state and validity. Unsupported or ambiguous targets fail before anything changes. An action whose effect is uncertain is never replayed automatically.

| Kind | Shape | Effect |
|---|---|---|
| `setText` | `{kind, value, clearFirst?}` | Replace (default) or append text |
| `setChecked` | `{kind, checked}` | Set a checkbox or radio |
| `selectOne` | `{kind, value}` | Select an option by value or visible label |
| `selectMany` | `{kind, values}` | Select several options |
| `setFiles` | `{kind, paths}` | Set file input paths. Needs `file:upload` |
| `clear` | `{kind}` | Empty the control |
| `activate` | `{kind}` | Activate a link or button |
| `passwordManagerFill` | `{kind}` | Watch a text, email or password field for the browser password manager's fill; the field is never clicked, focused or typed into. The `fill` and `completeForm` intents first click the manager's own prompt: on Chromium, 1Password's prompt frame directly; any other manager's prompt, found with vision. Needs `executionPolicy.passwordManager`; the filled value is never returned |
| `credential` | `{kind, name}` | Fill a password field with the secret of the [`[[credentials]]`](../guides/configuration.md#credentials) entry `name`, only when the field's page origin is that entry's `origin`. The field is set and gets `input` and `change` events; it is never clicked, focused or typed into. Needs `executionPolicy.passwordManager`; the secret is never returned, logged or journaled |

The `fill` and `completeForm` intents use the same vocabulary, without `activate`. See [Intent commands](../guides/intents.md).

In TypeScript, use `FormSnapshot`, `FORM_SNAPSHOT_SCHEMA_VERSION` and `isFormSnapshot` from `@cavi-ai/bobby-browser`.
