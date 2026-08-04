---
documentedVersion: {{PRODUCT_VERSION}}
---

# Intent commands

Semantic automation is available through the authenticated HTTP / TypeScript /
MCP surfaces when the principal holds `intent:execute`.

MCP exposes one tool per intent (`intent_locate`, `intent_fill`,
`intent_complete_form`, `intent_submit_and_verify`, `intent_wait_for_state`,
`intent_follow`, `intent_dismiss_obstruction`, `intent_extract`). They take
`sessionId` / `pageId` / the intent's own fields, plus optional `workflowId`
and `idempotencyKey`, and build the envelope server-side — see
[MCP tools](../surfaces/mcp-tools.md).

There are **no** dedicated intent HTTP routes. Over HTTP, and over MCP when you
need the escape hatch, submit via `POST /v1/commands` / `command_execute` /
`BrowserRuntimeClient.submit` with

```json
{ "kind": "intent", "input": { "kind": "<intent>", "input": { … } } }
```

inside a `CommandEnvelope` (`schemaVersion: 2`). TypeScript helpers:
`locateEnvelope`, `fillEnvelope`, `submitAndVerifyEnvelope`,
`waitForStateEnvelope`, `followEnvelope`, `dismissObstructionEnvelope`,
`extractEnvelope`. Multi-field forms use `completeFormRuntimeCommand` with
`intentEnvelope` (no dedicated `*Envelope` helper yet).

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
| `completeForm` | Reconciliable |
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
import { locateEnvelope } from "@cavi-ai/bobby-browser";
await client.submit(locateEnvelope(meta, "primary search box"), { idempotencyKey: crypto.randomUUID() });
```

Wire command: `{ kind: "intent", input: { kind: "locate", input: { purpose, hints } } }`.

### Fill (Reconciliable)

```ts
import { fillEnvelope } from "@cavi-ai/bobby-browser";
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

When `role` and exact `nearText` are supplied, `nearText` is the control's
accessible name while `purpose` remains the agent's task description. This
avoids requiring natural task phrasing to equal a page label. A fill completes
only when the worker returns value/upload postcondition evidence; an action
without verification evidence fails closed.

### Native constraint validity

After a successful type/select/check, fill verification also reads the
browser's native constraint-validity state (`willValidate` /
`validity.valid`). A value that was committed but violates `required`,
`pattern`, length, range, type, or other HTML constraints fails closed with
`verificationFailed`.

Evidence carries:

| Configuration key | Meaning |
|---|---|
| `formControlValid` | `"true"` / `"false"` |
| `formControlValidationMessage` | Browser message, bounded (≤1024 chars) |

Use the message to correct **only** the rejected field (especially inside
`completeForm`, which stops at the first failure and keeps prior field
evidence). Non-validating controls (`willValidate === false`) are treated as
valid for this check.

### CompleteForm (Reconciliable)

Apply an ordered, uniquely named list of fill fields as **one** intent.
Each field is resolved and verified before the next begins; execution stops
at the first failure and retains evidence for fields already attempted
(including a `completeFormField` configuration evidence entry per field name).
It never submits — use `submitAndVerify` (Boundary) afterward.

Constraints (compile / SDK reject before dispatch):

- `fields` non-empty, at most 128
- each `name` non-empty and unique within the form
- each field `purpose` and the form `purpose` obey intent purpose bounds
- any `files` field still requires `file:upload` on the bearer

```ts
import {
  completeFormRuntimeCommand,
  intentEnvelope,
  submitAndVerifyEnvelope,
} from "@cavi-ai/bobby-browser";

await client.submit(
  intentEnvelope(
    meta,
    completeFormRuntimeCommand({
      purpose: "applicant contact form",
      fields: [
        {
          name: "email",
          purpose: "enter the applicant email",
          hints: { role: "textbox", nearText: { kind: "exact", value: "Email address" } },
          value: { kind: "text", text: "a@example.com", clearFirst: true },
        },
        {
          name: "terms",
          purpose: "accept terms",
          hints: { role: "checkbox", nearText: { kind: "exact", value: "I agree" } },
          value: { kind: "checked", checked: true },
        },
      ],
    }),
  ),
  { idempotencyKey: crypto.randomUUID() },
);

await client.submit(
  submitAndVerifyEnvelope(meta, "submit application", { /* WaitForCommand expectedState */ }),
  { idempotencyKey: crypto.randomUUID() },
);
```

Wire command:
`{ kind: "intent", input: { kind: "completeForm", input: { purpose, fields } } }`
where each field is `{ name, purpose, hints?, value }`.

### SubmitAndVerify (Boundary)

```ts
import { submitAndVerifyEnvelope } from "@cavi-ai/bobby-browser";
await client.submit(
  submitAndVerifyEnvelope(meta, "submit login", { /* WaitForCommand expectedState */ }),
  { idempotencyKey: crypto.randomUUID() },
);
```

### WaitForState (Replayable)

```ts
import { waitForStateEnvelope } from "@cavi-ai/bobby-browser";
await client.submit(
  waitForStateEnvelope(meta, { /* WaitCondition */ }, 15_000),
  { idempotencyKey: crypto.randomUUID() },
);
```

### Follow

```ts
import { followEnvelope } from "@cavi-ai/bobby-browser";
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
import { dismissObstructionEnvelope } from "@cavi-ai/bobby-browser";
await client.submit(
  dismissObstructionEnvelope(meta, "dismiss cookie banner"),
  { idempotencyKey: crypto.randomUUID() },
);
```

### Extract (Replayable)

```ts
import { extractEnvelope } from "@cavi-ai/bobby-browser";
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

Vision-assisted resolution is **deny-by-default**. All three must pass:

1. Bearer holds `vision:assist`
2. Session created with `executionPolicy.visionAssist = true`
3. A provider is configured under `[vision]` (no `endpoint_url`, no escalation)

Otherwise vision escalation is denied (`VisionAssistDenied` / failed).

Capability + session grant is **not** sufficient for functional vision assist:
the configured endpoint must be **reachable** at runtime. A granted principal
and an opted-in session still fail closed when the provider is down or
misconfigured — `bobby doctor` warns when `[vision].endpoint_url` does not
accept a TCP connection.

When gates pass and deterministic resolution sticks, the engine captures a real
PNG via `screenshot_bytes` (Chromium and Firefox) and posts it to the provider.
Empty frames are not sent.

## Vision provider

Configure one HTTP provider in `config.toml` (also listed under
[Configuration](configuration.md#vision)):

```toml
[vision]
endpoint_url = "https://vision.example.test/propose" # https, or http on loopback only
token_env = "BOBBY_VISION_TOKEN"                  # env var holding the bearer (never in the file)
timeout_ms = 15000
```

### Local OpenAI via `bobby vision-proxy`

For local/dev with OpenAI, run the loopback adapter in a **second process**
before (or alongside) `bobby serve`:

```bash
export BOBBY_VISION_TOKEN=…   # bobby → proxy bearer
export OPENAI_API_KEY=…       # proxy → OpenAI
bobby vision-proxy            # default http://127.0.0.1:9100/vision
```

Then point config at the proxy:

```toml
[vision]
endpoint_url = "http://127.0.0.1:9100/vision"
token_env = "BOBBY_VISION_TOKEN"
timeout_ms = 15000
```

The proxy maps propose/extract requests to OpenAI and returns the wire shapes
below. Optional flags: `--bind`, `--path`, `--model`, `--openai-base-url`
(`--upstream` is `openai` only in v1).

**Manual check:** with both env vars set and `bobby vision-proxy` running,
uncomment the loopback `[vision]` block, start the runtime, open a session with
`visionAssist: true` under a principal with `vision:assist`, force a stuck
locate (or call `extract_structured`), and confirm escalation or structured
extract succeeds.

The runtime `POST`s JSON:

```json
{
  "purpose": "…",
  "intentKind": "locate",
  "stuck": "zeroCandidates",
  "screenshotPng": "<base64 PNG>"
}
```

and expects:

```json
{
  "confidence": 0.9,
  "action": { "kind": "click", "x": 12.0, "y": 34.0 }
}
```

`action.kind` is one of `click` (`x`,`y`), `typeText` (`text`), or
`extractValue` (`value`). Invalid responses, out-of-range confidence, oversized
bodies, and transport failures **decline** the escalation (fail closed).

Accepted proposals still require the engine's **0.75** confidence floor and
sha256-pinned verification before any browser action. That floor applies only
to vision *proposals* that drive browser actions — not to structured
extraction below.

### Structured extraction

The same `[vision]` endpoint also serves MCP `extract_structured` (HTTP /
TypeScript: primitive `extractStructured`). The runtime sends
`{schema, content, purpose}` (bounded page text) and the provider returns
`{"value": <json>}`. The runtime validates the value against the supplied JSON
schema and bounds it before it becomes `structuredExtraction` evidence — there
is no confidence floor or action verification on this path.
Gated like vision: `browser:mutate` + `vision:assist`, session
`executionPolicy.visionAssist`, and a configured `[vision]` provider.

## IntentHints

Optional disambiguation on most intents (`locate`, `fill`, `follow`, …). Wire
fields (camelCase):

| Field | Meaning |
|---|---|
| `role` | Accessible role hint |
| `nearText` | `TextMatch` (`exact` / `contains` / `regex`) near the control |
| `ordinal` | Zero-based index among same role/name peers (from snapshot targets) |
| `framePath` / `shadowPath` | Nested `TargetSpec` paths |
| `allowBestMatch` | Permit best-effort matching when set |

Copy snapshot targets with `intentHintsFromAccessibilityTarget` so `ordinal`
survives. IntentHints support ordinal; when you need the full `TargetSpec` on
a primitive, prefer MCP flat tools (omit `selector`) or HTTP/TS with
`selector: ""` beside `target` — see
[Accessibility snapshot](accessibility-snapshot.md).

## Purpose bounds

Intent `purpose` strings are non-empty and bounded (see
`MAX_INTENT_PURPOSE_BYTES` in the TypeScript SDK). Helpers call
`assertIntentPurpose`.
