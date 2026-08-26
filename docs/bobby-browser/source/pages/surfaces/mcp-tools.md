---
documentedVersion: {{PRODUCT_VERSION}}
---

# MCP tools reference

Both [MCP stdio](mcp-stdio.md) (`mcp-gateway`) and [MCP over HTTP](mcp-http.md)
(`POST /v1/mcp`) expose the same tool surface after a successful `initialize`.

## Protocol

- MCP protocol version: `2025-11-25`
- Call `initialize` before any tool listing or tool call
- Streamable HTTP: one JSON-RPC message per `POST`; `GET /v1/mcp` opens the
  SSE keep-alive channel (see [MCP over HTTP](mcp-http.md))
- Tool argument validation is bounded (stdio: ~1 MiB frames, 256 KiB tool input,
  event reads capped at 256 records)

## Tools

Tools are advertised only when the principal holds the required capability.

| Tool | Required capability | Purpose |
|---|---|---|
| `a11y_snapshot` | `browser:mutate` | Capture a compact accessibility tree with bounded form-control state, sensitive-value redaction, and command-ready semantic targets (`maxNodes` optional, 1…2048; default 256) |
| `checkpoint_save` | `recovery:write` | Persist a verified workflow checkpoint |
| `click` | `browser:mutate` | Click an element, optionally with native `shift`, `ctrl`, `alt`, or `meta` modifiers |
| `click_and_wait_for_popup` | `browser:mutate` | Click, wait for a `window.open` popup, and sync that page into `page_list` (auto-checkpoint boundary by default) |
| `command_execute` | `browser:mutate` | Execute one bounded `CommandEnvelope` |
| `context_ask` | `page:read` | Ask the retained page context where a described control is |
| `context_neighbors` | `context:read` | Show remembered form structure around a described control (siblings, success counters) |
| `control_action` | `browser:mutate` | Perform one typed native action against a semantic form-control target (Reconciliable) |
| `cookie_delete` | `browser:mutate` | Delete cookies by origin/name |
| `cookie_get` | `browser:mutate` | Read cookies (all origins or filtered) |
| `cookie_set` | `browser:mutate` | Store cookies |
| `dialog` | `browser:mutate` | Accept or dismiss the next JS dialog |
| `download_url` | `browser:mutate` + `file:download` | Download a URL with digest evidence |
| `emulate` | `browser:mutate` | Viewport + geolocation overrides |
| `evaluate_javascript` | `browser:mutate` + `javascript:evaluate` | Evaluate JavaScript (also session-policy gated) |
| `events_read` | `session:read` | Read retained events after a cursor |
| `extract_structured` | `browser:mutate` + `vision:assist` | Schema-shaped JSON extraction via the configured vision provider (also session `executionPolicy.visionAssist` + `[vision]`) |
| `form_snapshot` | `page:read` | Read the canonical bounded form inventory with sensitive-value redaction and no selectors, DOM IDs, or raw HTML (`maxControls` optional, 1…512; default 512) |
| `inspect` | `browser:mutate` | Read page state, optionally element-scoped |
| `intent_complete_form` | `browser:mutate` + `intent:execute` | Apply an ordered list of named fields as one intent; never submits (Reconciliable) |
| `intent_dismiss_obstruction` | `browser:mutate` + `intent:execute` | Dismiss a popup, overlay, or cookie banner (Reconciliable) |
| `intent_extract` | `browser:mutate` + `intent:execute` | Read named fields without mutating (Replayable) |
| `intent_fill` | `browser:mutate` + `intent:execute` | Fill one described control and verify the value (Reconciliable) |
| `intent_follow` | `browser:mutate` + `intent:execute` | Activate a link/control and verify the destination (Boundary when `boundary: true`) |
| `intent_locate` | `browser:mutate` + `intent:execute` | Locate an element by described purpose (Replayable) |
| `intent_submit_and_verify` | `browser:mutate` + `intent:execute` | Submit and verify the expected state (Boundary) |
| `intent_wait_for_state` | `browser:mutate` + `intent:execute` | Wait for a described page state (Replayable) |
| `job_cancel` | `job:cancel` | Cancel one owned job by id |
| `job_status` | `job:read` | Read one owned job by id |
| `job_submit` | `job:submit` | Submit a named job (`echo` / `sleep` / `http_probe` / `http_wait` / `http_fetch`; advertised in full/act/verify) |
| `navigate` | `browser:mutate` | Navigate a page to a URL |
| `network_log` | `browser:mutate` | Dump recorded network log as HAR |
| `page_activate` | `browser:mutate` | Bring a page to the front |
| `page_close` | `browser:mutate` | Close a page in an owned session |
| `page_list` | `browser:mutate` | List pages in an owned session |
| `page_open` | `page:write` (+ `browser:mutate` when `url` is supplied) | Open a page in an owned session and optionally navigate it |
| `pdf` | `browser:mutate` | Print the page to a PDF artifact |
| `recovery_status` | `recovery:read` | Read a workflow checkpoint and recovery receipts |
| `runtime_info` | `session:read` | Runtime capability and health information |
| `screenshot` | `browser:mutate` | Capture a screenshot artifact |
| `session_close` | `session:write` | Close a session and release its worker |
| `session_create` | `session:write` | Create a browser session |
| `session_list` | `session:read` | List sessions visible to the principal |
| `toolset_select` | none | Narrow `tools/list` to one phase |
| `type_text` | `browser:mutate` | Type text (optional `expectedUrl` page guard) |
| `upload_files` | `browser:mutate` + `file:upload` | Set files on a file input |
| `wait_for` | `browser:mutate` | Wait for a page condition |
| `workflow_observe` | `browser:mutate` (+ `page:read` when `includeForms` is true) | Read retained-first compact context, falling back to a live accessibility observation |
| `workflow_recover` | `recovery:write` | Recover a workflow from its verified checkpoint |
| `workflow_start` | `session:read` + `session:write` + `page:write` (+ `browser:mutate` when `url` is supplied) | Create and bind a session, page, and workflow in one lifecycle-safe call |

The flat browser tools (`navigate` … `evaluate_javascript` /
`extract_structured`, plus `page_activate` / `a11y_snapshot`) and the
`intent_*` tools build the command envelope for you (ids and deadline are
server-generated) and return the same `CommandOutcome` shape as
`command_execute`, including artifact / accessibility evidence.

`download_url.saveAs` may be relative to the configured downloads root or an
absolute file path directly under it. A completed download echoes that exact
validated input as `savedTo`; together with `sha256`, this is authoritative
landing and integrity evidence and does not require a shell check.
The advertised `maxBytes.maximum` is the effective runtime
`[http].max_download_bytes` value. Values outside `1..=maximum` fail before
network access with `invalidRequest` naming the exact limit; URL and
destination-policy failures remain `networkPolicyDenied`.

`control_action` accepts semantic `target` forms from `a11y_snapshot` and
`form_snapshot`, and one of `setText`, `setChecked`, `selectOne`,
`selectMany`, `setFiles`, `clear`, or `activate`. It returns typed reread
evidence; file paths and password values are never returned. `setFiles`
additionally requires `file:upload` at runtime.

Compact `intent_complete_form` evidence retains conditional
`revealedControls`, so their semantic targets can be used immediately. A
`networkQuiet` submit rejected by client-side validation returns compact,
value-free `formValidation` issues for the same repair loop.

`selectOne` and `selectMany` match by option value first and then visible label
(trimmed, case-insensitive) when no value matches.

`page_open` requires `sessionId` and optionally accepts `url`. With no URL it
returns the page state exactly as before. With a URL it opens and navigates in
one call, requires `browser:mutate`, and adds `navigationOutcome` to the page
state so navigation failure cannot be mistaken for a successfully loaded page.

`click`, `type_text`, and `upload_files` accept either a raw `selector` or a
semantic `target`. Targets returned by `a11y_snapshot` can be passed through
unchanged; a legacy selector is not required when `target` is present.
`upload_files` still requires `paths`.

`click.modifiers` is an optional unique array containing at most one each of
`shift`, `ctrl`, `alt`, and `meta`. Chromium and Firefox apply those keys during
the native pointer click and release them afterward. Modified clicks that enter
automatic download capture fail with `invalidRequest` instead of silently
dropping the requested modifiers.

`command_execute` still accepts nested intent envelopes
(`{ kind: "intent", input: { kind: "locate" \| … } }`) and remains the escape
hatch for anything the named tools do not cover. Skills are **not** MCP tools.

`click_and_wait_for_popup` is a boundary flow in one call when `autoCheckpoint`
is `true`. It can accept pinned `commandId` and `attemptId`, then persists a
checkpoint for the resulting page-affecting click in the same call. The command
also registers `window.open` targets into the current session page graph for
`page_list`, so the next authorization step can use those page IDs directly.

`wait_for` uses explicit discriminated `WaitCondition` objects:

| `kind` | Required fields |
|---|---|
| `element` | `target`, `state` |
| `text` | `target`, `matcher` |
| `value` | `target`, `matcher` |
| `url` | `matcher` |
| `document` | `ready` |
| `networkQuiet` | `idleMs`, `maxInFlight`, optional `ignoreUrlSubstrings`, `ignoreResourceTypes`, `ignoreLongLived` |

`matcher` uses `TextMatch` (`exact`, `contains`, `regex`) and `matcher.value`.

For `text` and `value` waits, role-scoped targets with `main|RootWebArea|document|application|generic|body`
or `css: body|html|:root` resolve against `document.body.innerText` via page evaluation.

Firefox handles `text`, `value`, `document`, `url`, and `element`; `networkQuiet` is
currently Chromium-only.

## Workflow continuity

Prefer `workflow_start` over separate `session_create` + `page_open` calls. It
creates the session and page, optionally navigates, and publishes the binding
only after setup succeeds:

```json
{
  "name": "workflow_start",
  "arguments": {"profile": "default", "url": "https://example.com"}
}
```

A successful `structuredContent` includes `status: "completed"`, an opaque
`workflowHandle`, `sessionId`, `pageId`, `workflowId`, the session and page
states, and `navigationOutcome` (null when no URL was supplied). A terminal
startup failure returns `workflowHandle: null`, cleanup state, and one of
`pageOpenFailed`, `navigationFailed`, `workflowGenerationChanged`, or
`workflowSupervisorLost`. A `pageOpenFailed` result has null `pageId` and
`page`; later failures retain both. In every failure, inspect `pageClosed`,
`sessionDeleted`, and `cleanupErrorCode`. If the response itself may have been
lost, do not blindly retry: call `session_list` first, then close or resume the
session that the first call may have created.

Example result without initial navigation:

```json
{
  "structuredContent": {
    "status": "completed",
    "workflowHandle": "wf_0123456789abcdef0123456789abcdef",
    "sessionId": "10000000-0000-4000-8000-000000000001",
    "pageId": "10000000-0000-4000-8000-000000000002",
    "workflowId": "10000000-0000-4000-8000-000000000003",
    "session": {
      "id": "10000000-0000-4000-8000-000000000001",
      "profile": "default",
      "proxy": null,
      "page_ids": ["10000000-0000-4000-8000-000000000002"],
      "created_at": "2026-08-07T12:00:00Z",
      "last_used_at": "2026-08-07T12:00:00Z",
      "execution_policy": {
        "javascriptEvaluation": false,
        "visionAssist": false,
        "fingerprint": false,
        "humanize": false
      }
    },
    "page": {
      "id": "10000000-0000-4000-8000-000000000002",
      "session_id": "10000000-0000-4000-8000-000000000001",
      "url": null,
      "mode": "Interactive",
      "ready_state": "complete",
      "pending_requests": 0
    },
    "navigationOutcome": null
  }
}
```

Use `workflow_observe` for compact retained-first context:

```json
{
  "name": "workflow_observe",
  "arguments": {
    "workflowHandle": "wf_0123456789abcdef0123456789abcdef",
    "goal": "Where is the checkout button?",
    "includeForms": true,
    "maxNodes": 256,
    "maxControls": 128
  }
}
```

Successful live observations default to `evidenceDetail: "compact"`, which
returns the accessibility evidence while leaving transport diagnostics in the
event stream. Pass `evidenceDetail: "full"` when debugging. For content work,
scope `target` to `{ "role": "main" }` to avoid paying repeatedly for site
navigation; omit the target when the task needs that navigation.

Its `structuredContent` reports `source: "retained"` when a `page:read`
context answer is available; otherwise it reports `source: "live"` with a
fresh accessibility `observationOutcome`. `includeForms: true` dynamically
requires `page:read` and adds `formSnapshot`; the base tool statically requires
`browser:mutate`.

Example live result without forms:

```json
{
  "structuredContent": {
    "status": "completed",
    "source": "live",
    "workflowHandle": "wf_0123456789abcdef0123456789abcdef",
    "sessionId": "10000000-0000-4000-8000-000000000001",
    "pageId": "10000000-0000-4000-8000-000000000002",
    "workflowId": "10000000-0000-4000-8000-000000000003",
    "retainedAnswer": null,
    "observationOutcome": {
      "status": "completed",
      "commandId": "10000000-0000-4000-8000-000000000004",
      "workflowId": "10000000-0000-4000-8000-000000000003"
    },
    "formSnapshot": null
  }
}
```

The handle can replace the explicit scope fields on this exact V1 allowlist:
`a11y_snapshot`, `click`, `click_and_wait_for_popup`, `context_ask`, `context_neighbors`,
`control_action`, `cookie_delete`, `cookie_get`, `cookie_set`, `dialog`,
`download_url`, `emulate`, `evaluate_javascript`, `extract_structured`,
`form_snapshot`, `inspect`, `intent_complete_form`,
`intent_dismiss_obstruction`, `intent_extract`, `intent_fill`,
`intent_follow`, `intent_locate`, `intent_submit_and_verify`,
`intent_wait_for_state`, `navigate`, `network_log`, `page_activate`,
`page_close`, `pdf`, `screenshot`, `type_text`, `upload_files`, and `wait_for`.
Each of these still accepts its complete explicit-ID form for compatibility.
Do not mix a handle with explicit scope fields: that is
`workflowBindingConflict`. A malformed, expired, unknown, or evicted handle is
`unknownWorkflowHandle`.

Handles are convenience bindings, not authority. Capability, operation, and
ownership checks still run, and explicit IDs remain the audit/repair path for
session/page lifecycle, checkpoints, and recovery. Successful `page_close` or
`session_close` automatically evicts matching local handles.

The handle registry is bounded to **64 committed LRU bindings plus 64
concurrent reservations**. A failed start never evicts a live handle; the 65th
successful committed binding evicts the least recently used old handle.
Before each start, the server authoritatively removes bindings for sessions
closed through another interface. A session's recorded `page_ids` are not
page-liveness truth: if only its page was closed elsewhere, the next page call
returns ordinary `notFound`, and the bounded committed-handle LRU eventually
reclaims that stale binding.

An accepted `initialize` starts a new server generation and invalidates every
handle from the previous one. Stdio normally has one server per process.

> **Important — shared HTTP generation.** Streamable HTTP currently caches one
> MCP `Server` per authenticated principal. Logical clients using the same
> principal share initialization and generation state: any accepted
> `initialize` resets all shared handles and returns the shared lifecycle to
> awaiting-initialized until a fresh `notifications/initialized`. Coordinate
> reinitialization, or use distinct principals when isolation is required,
> until the transport has a separate session key.

Every envelope-minting tool also accepts explicit `sessionId`, `pageId`, and an
optional `workflowId`, and returns the workflow ID it used. Omit `workflowId`
to mint one; pass it back to keep later commands in the same workflow.
`intent_*` tools also accept an optional `idempotencyKey`.

## Rejected arguments

A `-32602` response carries `data` describing what failed:

| `data.reason` | Extra fields | Meaning |
|---|---|---|
| `schemaViolation` | `pointer`, `constraint` | JSON Pointer to the offending argument and the schema keyword it violated |
| `malformedArguments` | — | Cleared the schema but failed to deserialize |
| `deadlineOutOfRange` | — | `command_execute` envelope deadline is past, or more than five minutes in the future |
| `invalidIdempotencyKey` | — | Key is not 1–128 printable ASCII characters |
| `workflowBindingConflict` | — | A handle-capable call mixed `workflowHandle` with explicit scope IDs; use one form only |
| `unknownWorkflowHandle` | — | Handle is malformed, unknown, evicted, or from an earlier server generation; repair with explicit IDs |

`pageOpenFailed`, `navigationFailed`, `workflowGenerationChanged`, and
`workflowSupervisorLost` are not protocol-layer rejections. They are the four
structured `workflow_start` terminal failure reasons; inspect the cleanup
fields and reconcile the returned/lost session with `session_list` and
explicit IDs before starting again.

`pointer` and `constraint` describe the published schema, never the submitted
value. Example: a `session_create` call with no `profile` returns
`{"reason":"schemaViolation","pointer":"/profile","constraint":"required"}`.

## Tool metadata

Every tool carries a human-readable `title` and MCP `annotations`
(`readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint`) so a
host can gate or confirm calls without knowing the tool vocabulary. Read-only
tools (`inspect`, `a11y_snapshot`, `cookie_get`, …) are marked
`readOnlyHint: true`; boundary tools `intent_submit_and_verify` and
`intent_follow` are marked `destructiveHint: true` (hosts may confirm before
calling; `intent_follow` is annotated conservatively even when `boundary` is
false). Close/delete tools (`session_close`, `page_close`, `cookie_delete`)
are also destructive.

Tools that return structured content declare an `outputSchema` (always
`type: object`), so a client can validate or render results without
reverse-engineering evidence shapes. A command whose outcome status is not
`completed` returns with MCP `isError: true` — check it before continuing a
flow. Failures carry a machine-readable repair hint: command-layer failures
set `error.repair`, RPC-layer rejections set `error.data.repair`, each
`{action, doc}` with `doc` pointing into `bobby://failure-taxonomy`. A
`needsReconciliation` outcome always carries the never-retry repair.

## Resources

Four static resources ship with the gateway, readable by any authenticated
principal (`resources/list` / `resources/read`; only live `artifact://`
entries are gated by `artifact:read`):

| URI | Contents |
|---|---|
| `bobby://capabilities` | What each capability gates, including double gates not visible in `tools/list` |
| `bobby://failure-taxonomy` | Error codes, protocol-layer rejections, and the repair action for each |
| `bobby://intents` | The eight intent tools: preconditions, class, and verification |
| `bobby://primitives` | The flat browser tools and the commands they mint |

Captured screenshots and downloads also become readable `artifact://<id>`
resources when the principal holds `artifact:capture`.

## Prompts

`prompts/list` / `prompts/get` expose three working loops, each taking
`sessionId` and `pageId` arguments:

- `fill_and_submit_form`
- `extract_from_page`
- `recover_workflow`

## Notifications

After `initialize`, the server pushes JSON-RPC notifications (no `id`) on the
same channel — stdio sessions are subscribed automatically; MCP over HTTP
delivers them on the `GET /v1/mcp` SSE stream:

| Method | Meaning |
|---|---|
| `notifications/bobby/event` | A runtime event for this principal, same body as `GET /v1/events` entries. Delivery follows the event-store cursor: fall behind retention and you get a gap, resync via `events_read` |
| `notifications/tools/list_changed` | The principal's capability set changed; the last `tools/list` is stale |

`initialize` advertises `tools.listChanged: true`; resources and prompts do
not change at runtime.

Compact accessibility trees (including form-control state):
[Accessibility snapshot](../guides/accessibility-snapshot.md).

Live JSON Schemas for tool arguments are defined in
`crates/mcp-gateway/src/schema.rs` (for example `session_create` requires
`profile` and accepts an optional `executionPolicy`, including the
`visionNode` selector that names a `[nodes.<name>]` vision node for this
session; `page_open` takes `sessionId` and optional `url`; `command_execute`
takes `envelope` and optional `idempotencyKey`). Each tool advertises only the `$defs` its own
arguments reach, so schemas stay self-contained without carrying the whole
type system. MCP argument names are camelCase even where
some HTTP request bodies use snake_case. The gateway's `schema_parity` tests
compare hand-bounded `kind` variant sets to schemars output from the
`types` crate (`schema` feature) so command/evidence drift fails CI.

## Lifecycle notes

- `runtime_info` reports `credentialExpiresAt` for the calling principal;
  `bobby doctor` warns under 7 days remaining. Its `capabilities` list names
  configured runtime features: `vision-assist` and `vision-provider` appear
  only when vision is wired — check them before vision-dependent tools,
  since `visionAssistFailed` with no provider configured never succeeds on
  retry
- Token rotate / revoke → re-`initialize` on the MCP session for that principal
- Stdio startup uses the four `AUTOMATION_RUNTIME_BOOTSTRAP_*` variables, not
  `AUTOMATION_RUNTIME_TOKEN` alone
- HTTP MCP uses `Authorization: Bearer …` with the client bearer

## Next

- [First browser session](../introduction/first-session.md)
- [Intent commands](../guides/intents.md)
- [Events and recovery](../guides/events-recovery.md)

## Toolset phases

`tools/list` for a principal holding every capability is ~127,000 bytes. An
agent that only needs part of the surface can narrow it with `toolset_select`:

| Phase | Contains | Payload |
|---|---|---|
| `explore` | read the page, navigate, wait, base controls (`click`, `type_text`, `control_action`, `upload_files`, `dialog`, `download_url`), plus `intent_complete_form` and `intent_submit_and_verify` — the standard form loop with no `toolset_select` first (default) | ~76 KB |
| `act` | escape hatches (`command_execute`, `evaluate_javascript`, `emulate`), niche mutations, and job tools | ~69 KB |
| `intent` | the `intent_*` family and `extract_structured` | ~74 KB |
| `verify` | evidence, checkpoints, recovery, job tools | ~41 KB |
| `full` | everything the principal's capabilities allow (including jobs when a job port is attached) | ~127 KB |

Session/page lifecycle, `runtime_info`, `toolset_select`, `workflow_start`, and
`workflow_observe` appear in every phase. This includes servers configured to
start directly in `act`, `intent`, or `verify`.

Clients that defer tool schemas should load `workflow_start`,
`workflow_observe`, `intent_complete_form`, and `intent_submit_and_verify`
together for the standard form loop. All four are available in the startup
`explore` phase, so no phase switch is needed.

Selecting a phase emits `notifications/tools/list_changed`; re-read
`tools/list` after calling it. Selecting the phase already in effect emits
nothing.

A phase changes what is **advertised**, never what is **permitted**. A tool
hidden by the current phase is still callable, and capability gates remain the
only authority over what a principal may do.

## Context questions

`context_ask` answers "where is the control described as X" from accessibility
snapshots the runtime already recorded, instead of returning a whole tree.

It returns a bound target and a confidence score, or nothing. Nothing is a real
answer, not an error: the retained context is invalidated by every command that
may have changed the page — including `navigate` and `emulate`, which are
replayable yet replace or reflow it — and by any non-read-only command that
failed. The repair is to take an `a11y_snapshot`, which re-populates it.

Ambiguous descriptions (two controls with the same accessible name), partial
matches, and anything below the confidence floor answer nothing rather than
guessing.
