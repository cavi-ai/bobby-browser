---
documentedVersion: 0.6.0
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
| `runtime_info` | `session:read` | Runtime capability and health information |
| `session_create` | `session:write` | Create a browser session |
| `session_close` | `session:write` | Close a session and release its worker |
| `session_list` | `session:read` | List sessions visible to the principal |
| `page_open` | `page:write` (+ `browser:mutate` when `url` is supplied) | Open a page in an owned session and optionally navigate it |
| `page_list` | `browser:mutate` | List pages in an owned session |
| `page_close` | `browser:mutate` | Close a page in an owned session |
| `page_activate` | `browser:mutate` | Bring a page to the front |
| `a11y_snapshot` | `browser:mutate` | Capture a compact accessibility tree with bounded form-control state, sensitive-value redaction, and command-ready semantic targets (`maxNodes` optional, 1…2048; default 256) |
| `form_snapshot` | `page:read` | Read the canonical bounded form inventory with sensitive-value redaction and no selectors, DOM IDs, or raw HTML (`maxControls` optional, 1…512; default 512) |
| `navigate` | `browser:mutate` | Navigate a page to a URL |
| `click` | `browser:mutate` | Click an element |
| `type_text` | `browser:mutate` | Type text (optional `expectedUrl` page guard) |
| `inspect` | `browser:mutate` | Read page state, optionally element-scoped |
| `screenshot` | `browser:mutate` | Capture a screenshot artifact |
| `wait_for` | `browser:mutate` | Wait for a page condition |
| `download_url` | `browser:mutate` + `file:download` | Download a URL with digest evidence |
| `upload_files` | `browser:mutate` + `file:upload` | Set files on a file input |
| `evaluate_javascript` | `browser:mutate` + `javascript:evaluate` | Evaluate JavaScript (also session-policy gated) |
| `extract_structured` | `browser:mutate` + `vision:assist` | Schema-shaped JSON extraction via the configured vision provider (also session `executionPolicy.visionAssist` + `[vision]`) |
| `command_execute` | `browser:mutate` | Execute one bounded `CommandEnvelope` |
| `control_action` | `browser:mutate` | Perform one typed native action against a semantic form-control target (Reconciliable) |
| `intent_locate` | `browser:mutate` + `intent:execute` | Locate an element by described purpose (Replayable) |
| `intent_fill` | `browser:mutate` + `intent:execute` | Fill one described control and verify the value (Reconciliable) |
| `intent_complete_form` | `browser:mutate` + `intent:execute` | Apply an ordered list of named fields as one intent; never submits (Reconciliable) |
| `intent_submit_and_verify` | `browser:mutate` + `intent:execute` | Submit and verify the expected state (Boundary) |
| `intent_wait_for_state` | `browser:mutate` + `intent:execute` | Wait for a described page state (Replayable) |
| `intent_follow` | `browser:mutate` + `intent:execute` | Activate a link/control and verify the destination (Boundary when `boundary: true`) |
| `intent_dismiss_obstruction` | `browser:mutate` + `intent:execute` | Dismiss a popup, overlay, or cookie banner (Reconciliable) |
| `intent_extract` | `browser:mutate` + `intent:execute` | Read named fields without mutating (Replayable) |
| `events_read` | `session:read` | Read retained events after a cursor |
| `checkpoint_save` | `recovery:write` | Persist a verified workflow checkpoint |
| `context_ask` | `page:read` | Ask the retained page context where a described control is |
| `context_neighbors` | `context:read` | Show remembered form structure around a described control (siblings, success counters) |
| `toolset_select` | none | Narrow `tools/list` to one phase |
| `recovery_status` | `recovery:read` | Read a workflow checkpoint and recovery receipts |
| `cookie_get` | `browser:mutate` | Read cookies (all origins or filtered) |
| `pdf` | `browser:mutate` | Print the page to a PDF artifact |
| `dialog` | `browser:mutate` | Accept or dismiss the next JS dialog |
| `emulate` | `browser:mutate` | Viewport + geolocation overrides |
| `network_log` | `browser:mutate` | Dump recorded network log as HAR |
| `cookie_set` | `browser:mutate` | Store cookies |
| `cookie_delete` | `browser:mutate` | Delete cookies by origin/name |
| `workflow_recover` | `recovery:write` | Recover a workflow from its verified checkpoint |
| `job_submit` | `job:submit` | Submit a named job (`echo` / `sleep` builtins; verify-phase advertise) |
| `job_status` | `job:read` | Read one owned job by id |
| `job_cancel` | `job:cancel` | Cancel one owned job by id |

The flat browser tools (`navigate` … `evaluate_javascript` /
`extract_structured`, plus `page_activate` / `a11y_snapshot`) and the
`intent_*` tools build the command envelope for you (ids and deadline are
server-generated) and return the same `CommandOutcome` shape as
`command_execute`, including artifact / accessibility evidence.

`control_action` accepts the exact `target` returned by `form_snapshot` and
one of `setText`, `setChecked`, `selectOne`, `selectMany`, `setFiles`, `clear`,
or `activate`. It returns typed reread evidence; file paths and password values
are never returned. `setFiles` additionally requires `file:upload` at runtime.

`page_open` requires `sessionId` and optionally accepts `url`. With no URL it
returns the page state exactly as before. With a URL it opens and navigates in
one call, requires `browser:mutate`, and adds `navigationOutcome` to the page
state so navigation failure cannot be mistaken for a successfully loaded page.

`click`, `type_text`, and `upload_files` accept either a raw `selector` or a
semantic `target`. Targets returned by `a11y_snapshot` can be passed through
unchanged; a legacy selector is not required when `target` is present.
`upload_files` still requires `paths`.

`command_execute` still accepts nested intent envelopes
(`{ kind: "intent", input: { kind: "locate" \| … } }`) and remains the escape
hatch for anything the named tools do not cover. Skills are **not** MCP tools.

## Workflow continuity

Every envelope-minting tool takes an optional `workflowId` and returns the
`workflowId` it used alongside the outcome. Omit it and the server mints one;
pass a returned value back to keep subsequent commands in the same workflow.
This is what makes `checkpoint_save`, `recovery_status`, and
`workflow_recover` reachable without hand-building envelopes.

`intent_*` tools also accept an optional `idempotencyKey`.

## Rejected arguments

A `-32602` response carries `data` describing what failed:

| `data.reason` | Extra fields | Meaning |
|---|---|---|
| `schemaViolation` | `pointer`, `constraint` | JSON Pointer to the offending argument and the schema keyword it violated |
| `malformedArguments` | — | Cleared the schema but failed to deserialize |
| `deadlineOutOfRange` | — | `command_execute` envelope deadline is past, or more than five minutes in the future |
| `invalidIdempotencyKey` | — | Key is not 1–128 printable ASCII characters |

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
flow.

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
  `bobby doctor` warns under 7 days remaining
- Token rotate / revoke → re-`initialize` on the MCP session for that principal
- Stdio startup uses the four `AUTOMATION_RUNTIME_BOOTSTRAP_*` variables, not
  `AUTOMATION_RUNTIME_TOKEN` alone
- HTTP MCP uses `Authorization: Bearer …` with the client bearer

## Next

- [First browser session](../introduction/first-session.md)
- [Intent commands](../guides/intents.md)
- [Events and recovery](../guides/events-recovery.md)

## Toolset phases

`tools/list` for a principal holding every capability is ~130,000 bytes. An
agent that only needs part of the surface can narrow it with `toolset_select`:

| Phase | Contains | Payload |
|---|---|---|
| `explore` | read the page, navigate, wait (default) | ~42 KB |
| `act` | raw primitives and `command_execute` | ~54 KB |
| `intent` | the `intent_*` family and `extract_structured` | ~74 KB |
| `verify` | evidence, checkpoints, recovery, job tools | ~49 KB |
| `full` | everything the principal's capabilities allow (except verify-only jobs) | ~130 KB |

Session and page lifecycle, `runtime_info`, and `toolset_select` itself appear
in every phase.

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
