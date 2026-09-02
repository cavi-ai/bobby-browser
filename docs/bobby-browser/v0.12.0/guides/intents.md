---
documentedVersion: 0.12.0
---

# Intent commands

Semantic automation is available through the authenticated HTTP / TypeScript /
MCP surfaces when the principal holds `intent:execute`.

MCP exposes one tool per intent (`intent_locate`, `intent_fill`,
`intent_complete_form`, `intent_submit_and_verify`, `intent_wait_for_state`,
`intent_follow`, `intent_dismiss_obstruction`, `intent_extract`,
`intent_solve_challenge`, `intent_detect_challenge`). They take
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
`extractEnvelope`, `solveChallengeEnvelope`, `detectChallengeEnvelope`.
Multi-field forms use `completeFormRuntimeCommand` with
`intentEnvelope` (no dedicated `*Envelope` helper yet).

Rust callers get the same builders from `bobby-browser-client`
(Supported tier): `locate_envelope`, `fill_envelope`,
`submit_and_verify_envelope`, `wait_for_state_envelope`,
`follow_envelope`, `dismiss_obstruction_envelope`, `extract_envelope`,
`solve_challenge_envelope`, `detect_challenge_envelope`, plus the
`*_runtime_command` builders and `intent_envelope` for the general case.

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
| `solveChallenge` | Reconciliable |
| `detectChallenge` | Replayable |
| `submitAndVerify` | Boundary |
| `follow` | Boundary if `boundary: true`, else Reconciliable |

`solveChallenge` drives the vision solve loop against a captcha or
verification widget (see `bobby vision solve`); `detectChallenge` only
classifies — screenshot in, `challengeDetection` evidence out, never an
action on the page. Detection is opt-in per call: nothing scans pages
automatically.

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
    { kind: "setText", value: "a@example.com", clearFirst: true },
    { role: "textbox", nearText: { kind: "exact", value: "Email address" } },
  ),
  { idempotencyKey: crypto.randomUUID() },
);
```

Unified `ControlAction` kinds for fill:

| Kind | Shape | Notes |
|---|---|---|
| `setText` | `{ kind: "setText", value, clearFirst? }` | Default path for textboxes; `clearFirst` defaults to true (replace) |
| `selectOne` | `{ kind: "selectOne", value }` | Matches option **value** first, then visible label (trimmed, case-insensitive) |
| `selectMany` | `{ kind: "selectMany", values }` | Multi-select only; matches by value or label |
| `setChecked` | `{ kind: "setChecked", checked: boolean }` | Checkbox / radio only |
| `setFiles` | `{ kind: "setFiles", paths }` | Requires `file:upload` |
| `clear` | `{ kind: "clear" }` | Clear field value |

`select` therefore resolves by value first so forms using explicit values remain stable,
then retries with trimmed visible-label matching before failing.

Checkbox / radio example:

```ts
await client.submit(
  fillEnvelope(
    meta,
    "accept terms",
    { kind: "setChecked", checked: true },
    { role: "checkbox", nearText: { kind: "exact", value: "I agree" } },
  ),
  { idempotencyKey: crypto.randomUUID() },
);
```

`setChecked` toggles via a real click when the control's state differs. Radios
may be selected (`checked: true`) but cannot be unchecked directly
(`checked: false` fails closed). Non-checkable targets must not use
`kind: "setChecked"`.

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

Resolution is just-in-time, so the list may include conditional fields that do
not exist when the form is first observed. Put each conditional field after the
field that reveals it; the engine resolves it against the updated page state
without requiring a second `completeForm` call.

`name` is the stable audit label for field evidence and, when `hints` is
empty, the exact accessible-name fallback. Explicit `hints` from
`form_snapshot` (normally `role` and `accessibleName`) override that fallback.

The named MCP tool defaults `evidenceDetail` to `compact` on success and
returns one filled-field summary. Full per-field evidence remains in runtime
events; pass `evidenceDetail: "full"` when diagnosing. Failures always retain
their detailed evidence so the caller can repair only the remaining fields.
Compact success evidence also retains any `revealedControls` created by a
conditional selection, including semantic targets that can be used without a
new form snapshot.

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
          value: { kind: "setText", value: "a@example.com", clearFirst: true },
        },
        {
          name: "terms",
          purpose: "accept terms",
          hints: { role: "checkbox", nearText: { kind: "exact", value: "I agree" } },
          value: { kind: "setChecked", checked: true },
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

With no hints, submit targeting defaults to the exact button named by
`purpose`, avoiding ancestor-text ambiguity. Explicit button hints from the
current snapshot remain authoritative.

When the confirmation copy or redirect is known, use a `text` or `url`
`expectedState` to prove that exact success state. When it is not known, use a
`networkQuiet` expected state. After the exactly-once boundary click settles,
the same call returns bounded `inspection` evidence and a
`submitSettlement` outcome:

- `settled` — the page settled with no visible `aria-invalid` controls
- `validationRejected` — correct the fields in compact `formValidation`
  evidence; each issue carries the control id, kind, accessible name, semantic
  target, and browser validity, but never its value or the rest of the form
  snapshot; do not blindly resubmit

No follow-up inspect is needed for the network-quiet path.

### WaitForState (Replayable)

```ts
import { waitForStateEnvelope } from "@cavi-ai/bobby-browser";
await client.submit(
  waitForStateEnvelope(meta, { /* WaitCondition */ }, 15_000),
  { idempotencyKey: crypto.randomUUID() },
);
```

### WaitCondition shape

`WaitForState` and MCP `wait_for` share the same `WaitCondition` shape:

| `kind` | Required fields |
|---|---|
| `element` | `target`, `state` |
| `text` | `target`, `matcher` |
| `value` | `target`, `matcher` |
| `url` | `matcher` |
| `document` | `ready` |
| `networkQuiet` | `idleMs`, `maxInFlight`, optional `ignoreUrlSubstrings`, `ignoreResourceTypes`, `ignoreLongLived` |

`matcher` is a `TextMatch` object: `{ kind: "exact" | "contains" | "regex", value }`.

`state` is one of `attached`, `detached`, `visible`, `hidden`, `enabled`, `disabled`.

`ready` is one of `commit`, `domContentLoaded`, `interactive`, `networkIdle`.

For `text` and `value`, role-based (`role: main|RootWebArea|document|application|generic|body`) and
`css: body|html|:root` targets read `document.body.innerText` so async confirmation text
checks align with whole-page assertions.

Firefox supports `text`, `value`, `document`, `url`, and `element`. `networkQuiet` is Chromium-only.

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

Note: `ExtractValueKind` (`text`, `attribute`, `href`) is separate from fill and control operations.

`ExtractValueKind`: `text`, `attribute` (+ `attribute` name), `href`.

### DetectChallenge (Replayable)

```ts
await client.submit(
  intentEnvelope(meta, {
    kind: "detectChallenge",
    input: { purpose: "check for a captcha blocking signup", hints: { timeoutMs: 15_000 } },
  }),
  { idempotencyKey: crypto.randomUUID() },
);
```

Completed evidence carries `{ kind: "challengeDetection", detection, priorKind? }`:
`detection` is the classified challenge (`challenge_type`, `confidence`,
`blocking`, optional `region`) or `null` when the page is provably clean;
`priorKind` names the site prior that enriched the prompt when one existed.
Detection carries no confidence floor — acting is what the floor protects.

## Vision double-gate

Vision-assisted resolution is **deny-by-default**. All three must pass:

1. Bearer holds `vision:assist`
2. Session created with `executionPolicy.visionAssist = true`
3. A reachable assist backend is configured — `[vision].endpoint_url`, a
   `[nodes]` vision node, or an ACP profile (`[vision].backend = "acp"`); no
   backend, no escalation

Otherwise vision escalation is denied (`VisionAssistDenied` / failed).

Capability + session grant is **not** sufficient for functional vision assist:
the configured endpoint must be **reachable** at runtime. A granted principal
and an opted-in session still fail closed when the provider is down or
misconfigured — `bobby doctor` warns on `vision-endpoint` reachability, and on
`vision-provider` / `vision-upstream-key` when the selected upstream profile is
missing or its required API key env is empty. Preferred local path:
[Configuration — Setup](configuration.md#setup-preferred).

When gates pass and deterministic resolution sticks, the engine captures a real
PNG via `screenshot_bytes` (Chromium and Firefox) and posts it to the backend.
Empty frames are not sent. Both engines execute the returned coordinates
natively — Chromium through CDP input, Firefox through BiDi pointer actions
against the bounded accessibility snapshot's candidates.

## Vision prefill

With `[vision].prefill = true` (default off), the first vision-eligible stuck
field in a `complete_form` does one screenshot and proposes for every
remaining field purpose, caching the results under the page's generation
discipline. Later stuck fields resolve from the cache with no extra
screenshots — one screenshot per stuck form instead of one per stuck field.

Evidence distinguishes the paths: `resolutionPath` is `visionPrefill` for a
cache-resolved field, `visionFallback` for a live stuck-rescue escalation,
`deterministic` when no vision ran. A cached proposal that fails to execute
is dropped and escalated live, never retried. Provider loss during a batch
records nothing and degrades to the ordinary path.

Only coordinate proposals are cached — a proposal carrying typed text is
never stored, in memory or otherwise.

## Vision backend

Two backends, selected by `[vision].backend`:

- **`direct`** (default) — Bobby posts propose/extract to the endpoint in
  `[vision]` (or a `[nodes.*.kind=vision]` node) and holds the upstream key.
- **`acp`** — Bobby delegates the vision task to an ACP harness that already
  owns the model login (Codex, Claude, OpenCode, Hermes, OpenClaw). Bobby never
  receives or stores that provider token. Each task runs in a new ACP child
  session with bounded text and image content, a strict JSON result,
  evidence-digest validation, and an explicit close. A harness that asks for
  interactive permission is cancelled and its child session closed.

```bash
bobby vision connect --yes --backend acp --provider codex \
  --command codex --arg acp --auth advertised
```

Both are configured in [Configuration](configuration.md#vision). A session
picks a named node with `executionPolicy.visionNode`.

## Vision provider

For the `direct` backend, upstream models are configured as named
OpenAI-compatible profiles under `[vision.providers]` — see
[Configuration](configuration.md#vision).

```toml
[vision]
endpoint_url = "http://127.0.0.1:9100/vision" # https, or http on loopback only
token_env = "BOBBY_VISION_TOKEN"              # env var holding the bearer (never in the file)
provider = "openai"
timeout_ms = 15000

[vision.providers.openai]
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"
```

### Local setup (preferred)

```bash
bobby vision connect --yes --provider openai   # or ollama / lmstudio / custom
export BOBBY_VISION_TOKEN=…
export OPENAI_API_KEY=…                        # when the profile sets api_key_env
bobby serve --vision                           # auto-spawns loopback vision-proxy
```

`bobby mcp-stdio --vision` uses the same spawn policy. `--no-vision` disables
spawn. Manual `bobby vision-proxy` in a second terminal remains valid
(`--bind`, `--path`, `--model`, `--openai-base-url`, `--api-key-env`).

**Manual check:** after connect + env exports, start with `--vision`, open a
session with `visionAssist: true` under a principal with `vision:assist`,
force a stuck locate (or call `extract_structured`), and confirm escalation
or structured extract succeeds.

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
