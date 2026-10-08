---
documentedVersion: {{PRODUCT_VERSION}}
---

# MCP tools

The tool catalog for agents. [MCP stdio](mcp-stdio.md) and [MCP over HTTP](mcp-http.md) serve the same tools after `initialize` (protocol `2025-11-25`).

A tool appears in `tools/list` only when the caller holds its capability. Argument names are camelCase.

## The working loop

```json
{"name": "workflow_start", "arguments": {"profile": "default", "url": "https://example.com"}}
```

`workflow_start` creates a session and a page, optionally navigates, and returns a `workflowHandle` plus `sessionId`, `pageId` and `workflowId`. Pass the handle to later calls instead of the IDs.

```json
{"name": "workflow_observe", "arguments": {"workflowHandle": "wf_0123456789abcdef0123456789abcdef", "goal": "Find the sign-in button", "includeForms": true}}
```

`workflow_observe` returns retained context when it can answer `goal`, otherwise a live accessibility tree. Pass `target: {"role": "main"}` to skip site navigation. `includeForms: true` adds a form snapshot and needs `page:read`. The default `evidenceDetail` is `compact`; use `full` to debug.

Act with `click`, `type_text`, `navigate`, or an `intent_*` tool, passing the same handle. Close the session with `session_close`.

If `workflow_start` fails, it reports `pageOpenFailed`, `navigationFailed`, `workflowGenerationChanged` or `workflowSupervisorLost`, with cleanup fields (`pageClosed`, `sessionDeleted`, `cleanupErrorCode`). If the response may have been lost, call `session_list` before retrying.

### Workflow handles

- A handle replaces `sessionId` and `pageId` on page-scoped tools. Do not send both: that is `workflowBindingConflict`.
- With exactly one live handle on the connection, a page-scoped call that names no scope uses it, and the evidence records `workflowHandleDefaulted`.
- `page_activate` with a handle and a `pageId` activates that page and rebinds the handle to it.
- After `click_and_wait_for_popup`, the handle follows the popup. If the popup later closes, read-only calls fall back to the opener page. Mutating calls fail with `popupClosed` evidence, so no effect is repeated.
- A handle is a convenience, not authority. Capability and ownership checks still apply. `initialize` invalidates all earlier handles. A malformed, unknown or expired handle is `unknownWorkflowHandle`; recover with explicit IDs.
- Streamable HTTP shares one server per principal, so a new `initialize` resets every handle of that principal. Use separate principals to isolate clients.

## Tools

| Tool | Capabilities | Purpose |
|---|---|---|
| `a11y_snapshot` | `browser:mutate` | Accessibility tree with a `target` per actionable node |
| `checkpoint_save` | `recovery:write` | Persist a verified workflow checkpoint |
| `click` | `browser:mutate` | Click a selector or target |
| `click_and_wait_for_download` | `browser:mutate`, `file:download` | Click and wait for the download; returns digest-verified artifact evidence |
| `click_and_wait_for_popup` | `browser:mutate` | Click and register the `window.open` popup in `page_list` |
| `command_execute` | `browser:mutate` | Submit one command envelope for anything the other tools do not cover |
| `context_ask` | `page:read` | Locate a described control from retained context |
| `context_neighbors` | `context:read` | Remembered form structure around a described control |
| `control_action` | `browser:mutate` | Native action on a form control target |
| `cookie_delete` | `browser:mutate` | Delete cookies by URL and name |
| `cookie_get` | `browser:mutate` | Read cookies visible to a page |
| `cookie_set` | `browser:mutate` | Store cookies |
| `dialog` | `browser:mutate` | Accept or dismiss the next JavaScript dialog |
| `download_url` | `browser:mutate`, `file:download` | Download a URL with digest evidence |
| `emulate` | `browser:mutate` | Viewport, mobile and geolocation overrides |
| `evaluate_javascript` | `browser:mutate`, `javascript:evaluate` | Evaluate a JavaScript expression |
| `events_read` | `session:read` | Read retained events after a cursor |
| `extract_structured` | `browser:mutate`, `vision:assist` | Schema-shaped JSON read by the vision provider |
| `form_snapshot` | `page:read` | Bounded inventory of form controls and their state |
| `inspect` | `browser:mutate` | Visible page text, optionally scoped to one element |
| `intent_complete_form` | `browser:mutate`, `intent:execute` | Fill an ordered list of named fields. Never submits |
| `intent_detect_challenge` | `browser:mutate`, `intent:execute`, `vision:assist` | Classify a captcha or verification challenge |
| `intent_dismiss_obstruction` | `browser:mutate`, `intent:execute` | Dismiss a popup, overlay or cookie banner |
| `intent_extract` | `browser:mutate`, `intent:execute` | Read named fields without changing the page |
| `intent_fill` | `browser:mutate`, `intent:execute` | Fill one described control and verify the value |
| `intent_follow` | `browser:mutate`, `intent:execute` | Activate a control and verify the resulting state |
| `intent_locate` | `browser:mutate`, `intent:execute` | Find an element without acting |
| `intent_solve_challenge` | `browser:mutate`, `intent:execute`, `vision:assist` | Run the vision solve loop on a challenge |
| `intent_submit_and_verify` | `browser:mutate`, `intent:execute` | Submit once and verify the expected state |
| `intent_wait_for_state` | `browser:mutate`, `intent:execute` | Wait for a described page state |
| `job_cancel` | `job:cancel` | Cancel an owned job |
| `job_status` | `job:read` | Read an owned job |
| `job_submit` | `job:submit` (+ `network:egress` for HTTP jobs) | Submit a built-in job |
| `navigate` | `browser:mutate` | Navigate a page to a URL |
| `network_log` | `browser:mutate` | Recorded network traffic as a HAR artifact |
| `page_activate` | `browser:mutate` | Bring a page to the front |
| `page_close` | `browser:mutate` | Close a page |
| `page_list` | `browser:mutate` | List a session's pages |
| `page_open` | `page:write` (+ `browser:mutate` with `url`) | Open a page, optionally navigating |
| `pdf` | `browser:mutate` | Print a page to a PDF artifact |
| `recovery_status` | `recovery:read` | Read a workflow's checkpoint and receipts |
| `runtime_info` | `session:read` | Runtime version, active sessions, credential expiry |
| `screenshot` | `browser:mutate` | Screenshot artifact |
| `session_close` | `session:write` | Close a session and release its browser |
| `session_create` | `session:write` | Create a session |
| `session_list` | `session:read` | List the caller's sessions |
| `toolset_select` | none | Narrow `tools/list` to one phase |
| `type_text` | `browser:mutate` | Type text into a selector or target |
| `upload_files` | `browser:mutate`, `file:upload` | Set files on a file input |
| `wait_for` | `browser:mutate` | Wait for a page condition |
| `workflow_observe` | `browser:mutate` (+ `page:read` with `includeForms`) | Read page context for a goal |
| `workflow_recover` | `recovery:write` | Resume or restart from the last verified checkpoint |
| `workflow_start` | `session:read`, `session:write`, `page:write` (+ `browser:mutate` with `url`) | Create session, page and workflow in one call |

## Parameters

### Sessions and pages

| Tool | Parameters |
|---|---|
| `session_create`, `workflow_start` | `profile` (required), `proxy`, `executionPolicy`, `zigzagzig`; `workflow_start` also takes `url` |
| `page_open` | `sessionId` (required), `url`. With `url` the result includes `navigationOutcome` |
| `page_list`, `page_close`, `page_activate` | `sessionId`, `pageId` or `workflowHandle` |
| `toolset_select` | `toolset`: `explore`, `act`, `intent`, `verify` or `full` |

`executionPolicy` flags are all off by default: `javascriptEvaluation`, `visionAssist`, `fingerprint`, `humanize`. `visionNode` names the vision node the session escalates to. `zigzagzig: true` creates a session with every flag on and automatic recovery; it is advertised only to principals holding `browser:fingerprint` and `browser:humanize`.

### Reading a page

| Tool | Parameters |
|---|---|
| `a11y_snapshot` | `maxNodes` 1 to 2048 (default 256), `target` to scope. See [Accessibility snapshot](../guides/accessibility-snapshot.md) |
| `form_snapshot` | `maxControls` 1 to 512. Passwords are redacted |
| `inspect` | `selector` or `target`, `includeHtml` |
| `screenshot` | `mode`: viewport, full page or element |
| `pdf` | `landscape`, `printBackground`, `scale` 0.1 to 2.0, `pageRanges` |
| `network_log` | `clear`. Recording starts at the first call; `clear: false` keeps the buffer |
| `context_ask`, `context_neighbors` | `description` (required, up to 256 bytes) |
| `wait_for` | `condition` and `timeoutMs` (both required) |
| `extract_structured` | `schema` (required), `purpose`. Needs `executionPolicy.visionAssist` |

`wait_for` conditions are objects with a `kind`:

| `kind` | Fields |
|---|---|
| `element` | `target`, `state` |
| `text`, `value` | `target`, `matcher` |
| `url` | `matcher` |
| `document` | `ready` |
| `networkQuiet` | `idleMs`, `maxInFlight`, optional `ignoreUrlSubstrings`, `ignoreResourceTypes`, `ignoreLongLived` |

`matcher` is `{"kind": "exact" | "contains" | "regex", "value": "..."}`.

### Acting on a page

| Tool | Parameters |
|---|---|
| `navigate` | `url` (required), `waitUntil`: `commit`, `domContentLoaded`, `interactive` or `networkIdle`; `timeoutMs` |
| `click` | `selector` or `target`; `modifiers` from `shift`, `ctrl`, `alt`, `meta`; `expectedUrl` |
| `click_and_wait_for_popup`, `click_and_wait_for_download` | `selector` or `target`, `timeoutMs`, `autoCheckpoint` |
| `type_text` | `value` (required), `selector` or `target`, `clearFirst`, `expectedUrl` |
| `control_action` | `target`, `action`: `setText`, `setChecked`, `selectOne`, `selectMany`, `setFiles`, `clear` or `activate` |
| `upload_files` | `paths` (required, up to 16), and `selector`, `target` or a form-snapshot `controlId` |
| `download_url` | `url`, `maxBytes` (both required), `saveAs`, `expectedContentType` |
| `dialog` | `action`: `accept` or `dismiss`; `timeoutMs` |
| `emulate` | `viewport`, `mobile`, `geolocation` |
| `cookie_get`, `cookie_delete` | `urls` (up to 64); `cookie_delete` also `names` |
| `cookie_set` | `cookies` (up to 128) |
| `evaluate_javascript` | `expression` (required), `timeoutMs`, `awaitPromise`. Needs `executionPolicy.javascriptEvaluation` |
| `command_execute` | `envelope` (required), `idempotencyKey` |

`click`, `type_text` and `upload_files` take either a CSS `selector` or a `target` of the form `{role, accessibleName, ordinal}`. Pass snapshot targets unchanged.

`download_url` saves under the configured downloads directory. `saveAs` is relative to it or an absolute path inside it. The result echoes `savedTo` and a `sha256`, which is the proof of the saved bytes. `maxBytes` must be between 1 and the advertised maximum (`[http].max_download_bytes`). Text-like downloads also return `downloadPreviews` of up to 4 KiB.

`selectOne` and `selectMany` match an option's value first, then its visible label.

Boundary tools (`click_and_wait_for_*`, `intent_follow` with `boundary`, `intent_submit_and_verify`) accept `autoCheckpoint` to checkpoint around a page-changing step. `click` and the `click_and_wait_for_*` tools accept pinned `commandId` and `attemptId`.

### Intents

Intent tools find controls by purpose and verify the result. Behavior and parameters are in [Intent commands](../guides/intents.md). All of them accept an optional `idempotencyKey`.

### Recovery, events and jobs

| Tool | Parameters |
|---|---|
| `checkpoint_save` | `checkpoint` (required), `evidenceRefs` |
| `recovery_status` | Exactly one of `workflowId` or `sessionId`; `limit` |
| `workflow_recover` | `workflowId` |
| `events_read` | `limit` (required), `cursor` |
| `job_submit` | `name` (required), `payload`, `priority`, `maxRetries`, `timeoutMs`. Built-in names: `echo`, `sleep`, `http_probe`, `http_wait`, `http_fetch` |
| `job_status`, `job_cancel` | `jobId` |

See [Events and recovery](../guides/events-recovery.md).

## Toolsets

`tools/list` starts with the `explore` set to keep the handshake small. Hidden tools are still callable.

| Toolset | Advertises |
|---|---|
| `explore` | Observation, lifecycle, navigation, click, type, upload, `intent_complete_form`, `intent_submit_and_verify` |
| `act` | Page-changing tools, `command_execute`, `evaluate_javascript`, jobs |
| `intent` | The `intent_*` family |
| `verify` | Evidence, checkpoints, recovery, jobs |
| `full` | Everything the caller may use |

Choose the starting set with `[mcp] startup_toolset` or `BOBBY_MCP_TOOLSET`. Session and page lifecycle tools and `toolset_select` appear in every set.

## Results and errors

A command whose outcome is not `completed` returns `isError: true`; check it before continuing. Failures carry a repair hint, `error.repair` for command failures and `error.data.repair` for protocol rejections, as `{action, doc}`. Every error message ends with the repair text.

An invalid call returns `-32602` with `data.reason`:

| `reason` | Meaning |
|---|---|
| `schemaViolation` | `pointer` and `constraint` name the offending argument and the schema keyword |
| `malformedArguments` | Passed the schema but could not be parsed |
| `deadlineOutOfRange` | `command_execute` deadline is past or more than five minutes ahead |
| `invalidIdempotencyKey` | Key is not 1 to 128 printable ASCII characters |
| `workflowBindingConflict` | A handle was sent together with explicit scope IDs |
| `unknownWorkflowHandle` | Handle is malformed, unknown, evicted or from an earlier connection |
| `hintsPerField` | `intent_complete_form`: top-level `hints` needs exactly one field without its own `hints` |
| `controlIdNotFound` | `upload_files`: `controlId` is not on the current form snapshot |
| `exactlyOneOfWorkflowIdOrSessionId` | `recovery_status` needs exactly one of the two |

Tool annotations (`readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint`) let hosts gate calls. `intent_submit_and_verify`, `intent_follow`, `session_close`, `page_close` and `cookie_delete` are marked destructive.

## Resources, prompts and notifications

| Resource | Contents |
|---|---|
| `bobby://capabilities` | What each capability gates |
| `bobby://failure-taxonomy` | Error codes and repair actions |
| `bobby://intents` | The intent tools, their preconditions and verification |
| `bobby://primitives` | The flat tools and the commands they create |
| `bobby://job-handlers` | Built-in job names and payloads |

Screenshots and downloads are readable as `artifact://<id>` resources when the caller holds `artifact:capture`. Screenshot results also include the image inline.

Prompts: `start_browsing` (optional `url`), `fill_and_submit_form`, `extract_from_page` and `recover_workflow`. The last three take `sessionId` and `pageId`; `recover_workflow` also takes `workflowId`.

After `initialize` the server pushes `notifications/bobby/event` for each runtime event of the caller and `notifications/tools/list_changed` when the caller's capabilities change. Over HTTP they arrive on the `GET /v1/mcp` stream.
