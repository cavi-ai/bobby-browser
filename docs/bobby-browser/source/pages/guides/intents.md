---
documentedVersion: 0.3.0
---

# Intent commands

Semantic automation is available through the authenticated HTTP / TypeScript /
MCP surfaces when the principal holds `intent:execute`. There are **no**
dedicated intent HTTP routes or MCP tools — submit via `POST /v1/commands` /
`command_execute` / `BrowserRuntimeClient.submit` with

```json
{ "kind": "intent", "input": { "kind": "<intent>", "input": { … } } }
```

inside a `CommandEnvelope` (`schemaVersion: 2`). TypeScript helpers:
`locateEnvelope`, `fillEnvelope`, `submitAndVerifyEnvelope`,
`waitForStateEnvelope`, `followEnvelope`, `dismissObstructionEnvelope`,
`extractEnvelope`.

## Command classes

| Class | Meaning |
|---|---|
| `Replayable` | Safe to retry under policy without a prior checkpoint |
| `Reconciliable` | May need inspection / reconcile before replay |
| `Boundary` | Mutating / side-effecting; checkpoint gate applies |

| Intent | Class |
|---|---|
| `locate` | Replayable |
| `waitForState` | Replayable |
| `extract` | Replayable |
| `fill` | Reconciliable |
| `dismissObstruction` | Reconciliable |
| `submitAndVerify` | Boundary |
| `follow` | Boundary if `boundary: true`, else Reconciliable |

## Envelope examples

Shared meta (ids are UUIDs; `deadline` RFC3339):

```ts
const meta = {
  commandId: crypto.randomUUID(),
  workflowId: crypto.randomUUID(),
  attemptId: crypto.randomUUID(),
  sessionId: session.id,
  pageId: page.id,
  deadline: new Date(Date.now() + 60_000).toISOString(),
};
```

### Locate (Replayable)

```ts
import { locateEnvelope } from "@bobby-browser/sdk";
await client.submit(locateEnvelope(meta, "primary search box"), { idempotencyKey: crypto.randomUUID() });
```

Wire command: `{ kind: "intent", input: { kind: "locate", input: { purpose, hints } } }`.

### Fill (Reconciliable)

```ts
import { fillEnvelope } from "@bobby-browser/sdk";
await client.submit(
  fillEnvelope(
    meta,
    "enter the applicant email",
    { kind: "text", text: "a@example.com", clearFirst: true },
    { role: "textbox", nearText: { kind: "exact", value: "Email address" } },
  ),
  { idempotencyKey: crypto.randomUUID() },
);
```

`FillValue` kinds:

| Kind | Shape | Notes |
|---|---|---|
| `text` | `{ kind: "text", text, clearFirst? }` | Default path for textboxes |
| `select` | `{ kind: "select", option }` | Exact option **value**, not label |
| `checked` | `{ kind: "checked", checked: boolean }` | Checkbox / radio only |
| `files` | `{ kind: "files", paths }` | Requires `file:upload` |

Checkbox / radio example:

```ts
await client.submit(
  fillEnvelope(
    meta,
    "accept terms",
    { kind: "checked", checked: true },
    { role: "checkbox", nearText: { kind: "exact", value: "I agree" } },
  ),
  { idempotencyKey: crypto.randomUUID() },
);
```

`checked` toggles via a real click when the control's state differs. Radios
may be selected (`checked: true`) but cannot be unchecked directly
(`checked: false` fails closed). Non-checkable targets must not use
`kind: "checked"`.

Use `completeForm` to apply an ordered, uniquely named list of fill fields as one reconciliable intent. Every field is resolved and verified before the next begins; execution stops at the first failure and retains evidence for fields already attempted. It never submits the form implicitly—submission remains a separate boundary intent.
When `role` and exact `nearText` are supplied, `nearText` is the control's
accessible name while `purpose` remains the agent's task description. This
avoids requiring natural task phrasing to equal a page label. A fill completes
only when the worker returns value/upload postcondition evidence; an action
without verification evidence fails closed.

### SubmitAndVerify (Boundary)

```ts
import { submitAndVerifyEnvelope } from "@bobby-browser/sdk";
await client.submit(
  submitAndVerifyEnvelope(meta, "submit login", { /* WaitForCommand expectedState */ }),
  { idempotencyKey: crypto.randomUUID() },
);
```

### WaitForState (Replayable)

```ts
import { waitForStateEnvelope } from "@bobby-browser/sdk";
await client.submit(
  waitForStateEnvelope(meta, { /* WaitCondition */ }, 15_000),
  { idempotencyKey: crypto.randomUUID() },
);
```

### Follow

```ts
import { followEnvelope } from "@bobby-browser/sdk";
await client.submit(
  followEnvelope(meta, "docs link", { /* expectedDestination WaitForCommand */ }, { boundary: false }),
  { idempotencyKey: crypto.randomUUID() },
);
```

Set `boundary: true` when activation may mutate (for example sign-out); requires a
matching workflow checkpoint.

### DismissObstruction (Reconciliable)

Clears a popup / overlay / cookie banner. No caller `boundary` flag — always
reconciliable. Default `timeoutMs` is 5000.

```ts
import { dismissObstructionEnvelope } from "@bobby-browser/sdk";
await client.submit(
  dismissObstructionEnvelope(meta, "dismiss cookie banner"),
  { idempotencyKey: crypto.randomUUID() },
);
```

### Extract (Replayable)

```ts
import { extractEnvelope } from "@bobby-browser/sdk";
await client.submit(
  extractEnvelope(meta, "product fields", [
    { name: "title", purpose: "product title", value: { kind: "text" } },
    { name: "link", purpose: "product link", value: { kind: "href" } },
  ]),
  { idempotencyKey: crypto.randomUUID() },
);
```

`ExtractValueKind`: `text`, `attribute` (+ `attribute` name), `href`.

## Vision double-gate

Vision-assisted resolution is **deny-by-default**. Both must pass:

1. Bearer holds `vision:assist`
2. Session created with `executionPolicy.visionAssist = true`

Otherwise vision escalation is denied.

## Purpose bounds

Intent `purpose` strings are non-empty and bounded (see
`MAX_INTENT_PURPOSE_BYTES` in the TypeScript SDK). Helpers call
`assertIntentPurpose`.
