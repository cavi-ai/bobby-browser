---
documentedVersion: {{PRODUCT_VERSION}}
---

# Intent commands

An intent is a goal-level step such as "fill the email field" or "submit the form and confirm the order page loaded". You describe the control by purpose and accessible hints. bobby finds it, acts, and verifies the result before returning evidence. Use intents instead of raw clicks when you want a step that fails loudly if it did not work.

Intents need the `intent:execute` capability.

## Run an intent

Over MCP, call the matching tool:

```json
{"name": "intent_fill", "arguments": {
  "workflowHandle": "wf_0123456789abcdef0123456789abcdef",
  "purpose": "enter the applicant email",
  "hints": {"role": "textbox", "accessibleName": "Email address"},
  "value": {"kind": "setText", "value": "ada@example.com"}
}}
```

Over HTTP, wrap the intent in a command envelope and send it to `POST /v1/commands`:

```json
{"kind": "intent", "input": {"kind": "fill", "input": {"purpose": "enter the applicant email", "hints": {"role": "textbox", "accessibleName": "Email address"}, "value": {"kind": "setText", "value": "ada@example.com"}}}}
```

The TypeScript SDK builds envelopes with `locateEnvelope`, `fillEnvelope`, `submitAndVerifyEnvelope`, `waitForStateEnvelope`, `followEnvelope`, `dismissObstructionEnvelope`, `extractEnvelope`, `detectChallengeEnvelope` and `solveChallengeEnvelope`. For forms, use `completeFormRuntimeCommand` with `intentEnvelope`. The Rust client has the same builders in snake case.

```ts
import { fillEnvelope } from "@cavi-ai/bobby-browser";

await client.submit(
  fillEnvelope(meta, "enter the applicant email",
    { kind: "setText", value: "ada@example.com" },
    { role: "textbox", nearText: { kind: "exact", value: "Email address" } }),
  { idempotencyKey: crypto.randomUUID() },
);
```

`meta` holds `commandId`, `workflowId`, `attemptId`, `sessionId`, `pageId` and `deadline`.

## The intents

| Intent | MCP tool | Class | Does |
|---|---|---|---|
| `locate` | `intent_locate` | Replayable | Finds a control and returns its fingerprint |
| `fill` | `intent_fill` | Reconciliable | Sets one control and verifies the value |
| `completeForm` | `intent_complete_form` | Reconciliable | Fills an ordered list of fields. Never submits |
| `submitAndVerify` | `intent_submit_and_verify` | Boundary | Submits once and verifies `expectedState` |
| `follow` | `intent_follow` | Reconciliable, Boundary with `boundary: true` | Activates a control and verifies the result |
| `waitForState` | `intent_wait_for_state` | Replayable | Waits for a page condition |
| `dismissObstruction` | `intent_dismiss_obstruction` | Reconciliable | Closes a popup, overlay or cookie banner |
| `extract` | `intent_extract` | Replayable | Reads named fields without changing the page |
| `detectChallenge` | `intent_detect_challenge` | Replayable | Classifies a captcha or verification challenge |
| `solveChallenge` | `intent_solve_challenge` | Reconciliable | Runs the vision solve loop on a challenge |

The class tells you how to retry. A Replayable intent is safe to repeat. A Reconciliable intent may need a page check before repeating. A Boundary intent changes the world and takes a checkpoint first; after a failure with `needsReconciliation`, inspect the page instead of retrying. See [Events and recovery](events-recovery.md).

## Hints

`hints` narrow which control matches:

| Field | Meaning |
|---|---|
| `role` | Accessible role |
| `accessibleName` | Accessible name. May be a `controlId` from `form_snapshot` |
| `nearText` | `{kind: "exact" \| "contains" \| "regex", value}` near the control |
| `ordinal` | Zero-based index among peers with the same role and name |
| `framePath`, `shadowPath` | Paths into frames and shadow roots |
| `allowBestMatch` | Accept a best-effort match |

Copy a snapshot node's `target` into hints so `ordinal` is kept. In TypeScript, use `intentHintsFromAccessibilityTarget(node.target)`. With `role` and exact `nearText`, `nearText` is the accessible name and `purpose` stays a free-text task description. A `purpose` is required and bounded. With no hints, `submitAndVerify` targets the button named by `purpose`.

## Fill values

`fill` and each `completeForm` field take a `value` with a `kind`:

| Kind | Shape | Notes |
|---|---|---|
| `setText` | `{value, clearFirst?}` | `clearFirst` defaults to true |
| `selectOne` | `{value}` | Matches option value first, then visible label |
| `selectMany` | `{values}` | Multi-select |
| `setChecked` | `{checked}` | Checkbox or radio. A radio cannot be unchecked |
| `setFiles` | `{paths}` | Needs `file:upload` |
| `clear` | none | Empties the field |

A fill succeeds only with postcondition evidence. It also reads the browser's constraint validity. A committed value that violates `required`, `pattern`, a range or a type fails with `verificationFailed`. The evidence has `formControlValid` and `formControlValidationMessage`; fix that field and retry.

## Fill a whole form

`completeForm` resolves each field just before filling it, so a conditional field can follow the field that reveals it.

```json
{"name": "intent_complete_form", "arguments": {
  "workflowHandle": "wf_0123456789abcdef0123456789abcdef",
  "purpose": "applicant contact form",
  "fields": [
    {"name": "email", "purpose": "enter the applicant email",
     "hints": {"role": "textbox", "accessibleName": "Email address"},
     "value": {"kind": "setText", "value": "ada@example.com"}},
    {"name": "terms", "purpose": "accept terms",
     "hints": {"role": "checkbox", "accessibleName": "I agree"},
     "value": {"kind": "setChecked", "checked": true}}
  ]
}}
```

- `fields` has 1 to 128 entries. Each `name` is unique and labels the evidence. If `name` is a `controlId` from `form_snapshot` and a field has no hints, bobby uses that control.
- Execution stops at the first failed field. The evidence keeps the fields already filled, so retry only the rest.
- Success evidence is compact by default and lists `revealedControls` with usable targets. Pass `evidenceDetail: "full"` to debug.
- The MCP tool accepts top-level `hints` only when `fields` has exactly one entry without its own hints. Otherwise it fails with `hintsPerField`.

## Submit and verify

```json
{"name": "intent_submit_and_verify", "arguments": {
  "workflowHandle": "wf_0123456789abcdef0123456789abcdef",
  "purpose": "Place order",
  "expectedState": {"condition": {"kind": "url", "matcher": {"kind": "contains", "value": "/confirmed"}}, "timeoutMs": 30000}
}}
```

`expectedState` is required. Use a `text` or `url` condition when you know the success state, or `networkQuiet` when you do not. With `networkQuiet` the result has `submitSettlement`:

- `settled`: the page went quiet with no invalid controls.
- `validationRejected`: fix the fields listed in `formValidation` (control, kind, name, target, validity, never values) and do not resubmit blindly.

A second submit in the same workflow fails with `boundaryAlreadyExecuted` unless you pass `reSubmit: true`.

## Wait conditions

`waitForState` and the `wait_for` tool share one condition shape. See the table in [MCP tools](../surfaces/mcp-tools.md#reading-a-page). `state` is one of `attached`, `detached`, `visible`, `hidden`, `enabled`, `disabled`. `ready` is one of `commit`, `domContentLoaded`, `interactive`, `networkIdle`.

## Follow

`intent_follow` replaces a click followed by a wait. Give it `expectedState` (alias `expectedDestination`; send exactly one). Set `boundary: true` when the activation changes state, such as signing out. Evidence is compact by default.

## Extract

```json
{"name": "intent_extract", "arguments": {
  "workflowHandle": "wf_0123456789abcdef0123456789abcdef",
  "purpose": "read the product details",
  "fields": [
    {"name": "title", "purpose": "product title", "value": {"kind": "text"}},
    {"name": "link", "purpose": "product link", "value": {"kind": "href"}}
  ]
}}
```

Value kinds are `text`, `attribute` (with an attribute name) and `href`. A field that does not resolve is reported on that field, not as a call failure. Fields target actionable elements. To read plain page text, use `inspect` or `extract_structured`.

## Vision assist

When deterministic matching is stuck, an intent can ask a vision provider to pick a control from a screenshot. This is off unless all three hold:

1. The caller holds `vision:assist`.
2. The session was created with `executionPolicy.visionAssist = true`.
3. A vision backend is configured. See [Configuration](configuration.md#vision).

The provider chooses among up to five near-miss candidates, and bobby accepts a proposal only above a 0.75 confidence floor and after verifying it. With `[vision].prefill = true` (the default), `intent_complete_form` resolves unresolved fields from one screenshot before it starts. Evidence marks the path as `deterministic`, `visionPrefill` or `visionFallback`.

`intent_detect_challenge` classifies a captcha and never acts: its `challengeDetection` evidence holds the type, confidence and whether it blocks, or `null` for a clean page. `intent_solve_challenge` runs the solve loop until the challenge clears or `timeoutMs` passes. bobby does not scan pages for challenges on its own.
