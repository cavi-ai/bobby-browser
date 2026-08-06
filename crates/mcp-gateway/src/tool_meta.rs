//! Capability, operation, and description tables for MCP tools.
//!
//! Host annotations live in `annotations.rs`. These maps are the enforcement
//! and agent-facing description authority for `tools/list` / `tools/call`.

pub(crate) fn required_capabilities(name: &str) -> Option<&'static [types::Capability]> {
    match name {
        "checkpoint_save" | "workflow_recover" => Some(&[types::Capability::RecoveryWrite]),
        "recovery_status" => Some(&[types::Capability::RecoveryRead]),
        "command_execute" | "control_action" | "navigate" | "click" | "type_text" | "inspect"
        | "screenshot" | "wait_for" | "page_list" | "page_close" | "page_activate"
        | "a11y_snapshot" | "pdf" | "dialog" | "emulate" | "network_log" | "cookie_get"
        | "cookie_set" | "cookie_delete" => Some(&[types::Capability::BrowserMutate]),
        "extract_structured" => Some(&[
            types::Capability::BrowserMutate,
            types::Capability::VisionAssist,
        ]),
        "download_url" => Some(&[
            types::Capability::BrowserMutate,
            types::Capability::FileDownload,
        ]),
        "upload_files" => Some(&[
            types::Capability::BrowserMutate,
            types::Capability::FileUpload,
        ]),
        "evaluate_javascript" => Some(&[
            types::Capability::BrowserMutate,
            types::Capability::JavascriptEvaluate,
        ]),
        "intent_complete_form"
        | "intent_dismiss_obstruction"
        | "intent_extract"
        | "intent_fill"
        | "intent_follow"
        | "intent_locate"
        | "intent_submit_and_verify"
        | "intent_wait_for_state" => Some(&[
            types::Capability::BrowserMutate,
            types::Capability::IntentExecute,
        ]),
        "events_read" | "runtime_info" | "session_list" => Some(&[types::Capability::SessionRead]),
        "context_ask" => Some(&[types::Capability::PageRead]),
        "context_neighbors" => Some(&[types::Capability::ContextRead]),
        // Grants nothing, so it needs nothing beyond an authenticated
        // connection. A capability gate here could strand a principal in a
        // phase it lacked the capability to leave.
        "toolset_select" => Some(&[]),
        "form_snapshot" => Some(&[types::Capability::PageRead]),
        "page_open" => Some(&[types::Capability::PageWrite]),
        "session_close" | "session_create" => Some(&[types::Capability::SessionWrite]),
        "job_submit" => Some(&[types::Capability::JobSubmit]),
        "job_status" => Some(&[types::Capability::JobRead]),
        "job_cancel" => Some(&[types::Capability::JobCancel]),
        _ => None,
    }
}

pub(crate) fn required_operation(name: &str) -> Option<types::InterfaceOperation> {
    match name {
        "checkpoint_save" => Some(types::InterfaceOperation::CreateCheckpoint),
        "recovery_status" => Some(types::InterfaceOperation::ReadCheckpoint),
        "command_execute"
        | "control_action"
        | "navigate"
        | "click"
        | "type_text"
        | "inspect"
        | "screenshot"
        | "wait_for"
        | "page_list"
        | "page_close"
        | "page_activate"
        | "a11y_snapshot"
        | "extract_structured"
        | "pdf"
        | "dialog"
        | "emulate"
        | "network_log"
        | "cookie_get"
        | "cookie_set"
        | "cookie_delete"
        | "download_url"
        | "upload_files"
        | "evaluate_javascript"
        | "intent_complete_form"
        | "intent_dismiss_obstruction"
        | "intent_extract"
        | "intent_fill"
        | "intent_follow"
        | "intent_locate"
        | "intent_submit_and_verify"
        | "intent_wait_for_state" => Some(types::InterfaceOperation::SubmitCommand),
        "events_read" => Some(types::InterfaceOperation::SubscribeEvents),
        "context_ask" => Some(types::InterfaceOperation::ReadPage),
        "context_neighbors" => Some(types::InterfaceOperation::ReadContext),
        "form_snapshot" => Some(types::InterfaceOperation::ReadPage),
        "page_open" => Some(types::InterfaceOperation::OpenPage),
        "runtime_info" => Some(types::InterfaceOperation::RuntimeInfo),
        "session_close" => Some(types::InterfaceOperation::DeleteSession),
        "session_create" => Some(types::InterfaceOperation::CreateSession),
        "session_list" => Some(types::InterfaceOperation::ReadSession),
        "workflow_recover" => Some(types::InterfaceOperation::RecoverWorkflow),
        "job_submit" => Some(types::InterfaceOperation::SubmitJob),
        "job_status" => Some(types::InterfaceOperation::ReadJob),
        "job_cancel" => Some(types::InterfaceOperation::CancelJob),
        _ => None,
    }
}

pub(crate) fn tool_description(name: &str) -> &'static str {
    match name {
        "context_ask" => "Ask the retained page context where a described control is, instead of pulling a whole accessibility tree into your context. Requires page:read. Returns a bound target and a confidence score, or nothing. On no answer, take an a11y_snapshot -- the context is invalidated by every command that may have changed the page.",
        "context_neighbors" => "Show the remembered form structure around a described control: its form, sibling controls, and per-intent success counters, marked as remembered rather than live-observed. Requires context:read. Returns nothing for an unknown site or control.",
        "toolset_select" => "Narrow tools/list to one phase: explore (default), act, intent, verify, or full. Requires no capability. Emits notifications/tools/list_changed, so re-read tools/list after calling it. Hidden tools stay callable; this changes what is advertised, not what is permitted.",
        "runtime_info" => "Runtime version, granted capabilities, active session count, uptime, and credential expiry. Requires session:read.",
        "session_list" => "List browser sessions visible to this principal, each with its profile and open-page count. Requires session:read.",
        "page_list" => "List open pages in an owned session, each with its id, URL, and title. Requires browser:mutate.",
        "inspect" => "Read a page's text, optionally scoped to one element by selector or target, with HTML on request. Requires browser:mutate.",
        "a11y_snapshot" => "Capture a compact accessibility tree for a page, capped at 2048 nodes, with command-ready targets on actionable nodes. Requires browser:mutate. Start here: pass a node's target into an intent_* tool rather than guessing a selector.",
        "form_snapshot" => "Read a bounded, engine-neutral inventory of a page's form controls without exposing selectors or sensitive values. Requires page:read.",
        "screenshot" => "Capture a screenshot artifact of a page's viewport, full page, or one element. Requires browser:mutate.",
        "events_read" => "Read retained runtime events for this principal after a cursor, bounded by a limit. Requires session:read. Long-polls: it blocks until an event past the cursor arrives or the request deadline expires (about 60s), so it is not a quick read. The notifications/bobby/event channel pushes the same frames without polling -- see bobby://failure-taxonomy.",
        "recovery_status" => "Read a workflow's checkpoint and recovery receipts without attempting recovery, or pass sessionId instead of workflowId to list that session's recoverable workflows newest-first. Pass exactly one. Requires recovery:read.",
        "cookie_get" => "Read cookies visible to a page, optionally filtered by URL. Requires browser:mutate.",
        "checkpoint_save" => "Persist a verified workflow checkpoint. Requires recovery:write. Pass evidenceRefs -- command ids whose evidence the runtime resolves from its journal. For a Boundary command, save BEFORE it with recoveryClass boundary and boundaryCommandId/attemptId equal to the ids you pass that command. On failure with a missing command id, confirm the command completed first.",
        "workflow_recover" => "Recover a workflow from its last verified checkpoint, resuming, restarting, or flagging reconciliation. Requires recovery:write. Produces recovery evidence and the decision reached. On failure with notFound, this principal doesn't own the checkpoint's session (it may be closed) -- verify with session_list. A missing checkpoint itself surfaces as an opaque internal error, not notFound.",
        "session_create" => "Create a browser session with a profile, optional proxy, and execution policy. Requires session:write. Produces the session's id and initial state. On failure with resourceExhausted, this principal already holds its session limit -- close an idle one first.",
        "session_close" => "Close a session and release its pages, workers, and artifacts. Requires session:write. Destructive: in-flight commands on the session are cancelled. On failure, the session may already be closed -- confirm with session_list.",
        "page_open" => "Open a page in an owned session, optionally navigating it to a URL in the same call. Requires page:write, and browser:mutate too if a URL is given. Produces the page's id and, if navigated, navigation evidence. On failure with notFound, the session is not owned by this principal -- check session_list.",
        "page_close" => "Close a page in an owned session. Requires browser:mutate. Destructive: the page and its in-flight commands are gone immediately. On failure with notFound, the page id is stale -- call page_list for current ids.",
        "page_activate" => "Bring a page to the front in an owned session. Requires browser:mutate. Produces the activated page's URL and title. On failure with notFound, the page id is stale -- call page_list for current ids.",
        "navigate" => "Navigate a page to a URL and wait for the requested load state. Requires browser:mutate. Produces navigation evidence with the settled URL and title. On failure with invalidRequest, the URL scheme isn't http(s) or data -- use one of those; on deadlineExceeded, retry with a longer timeout_ms.",
        "click" => "Click an element identified by a selector or a resolved target. Requires browser:mutate. Produces execution-path evidence for the click. When boundary is true, autoCheckpoint defaults to true and mints the required checkpoint in the same call (pass false to author your own). On failure with targetNotFound or targetAmbiguous, take a fresh a11y_snapshot and pass the new target.",
        "type_text" => "Type text into an element identified by a selector or a resolved target, optionally clearing it first. Requires browser:mutate. Produces execution-path evidence for the input. On failure with targetNotFound or targetAmbiguous, take a fresh a11y_snapshot and pass the new target.",
        "wait_for" => "Wait for a page condition with a bounded timeout. Requires browser:mutate. Produces wait evidence with elapsed time and observation count. On failure with waitConditionTimedOut, confirm the condition still matches page state via inspect, then retry with a longer timeout.",
        "control_action" => "Perform one typed native form-control action and return the reread control state. Requires browser:mutate, and file:upload too if the action is setFiles. Produces control-action evidence with the post-action value. On failure with targetNotFound, take a fresh form_snapshot and pass the new target.",
        "emulate" => "Set viewport size, mobile mode, and geolocation overrides for a page. Requires browser:mutate. Produces emulation evidence confirming the applied overrides. On failure with invalidRequest, viewport or coordinates are out of range -- keep width/height within 1-16384 and coordinates within valid bounds.",
        "dialog" => "Accept or dismiss the next JavaScript dialog on a page within a timeout. Requires browser:mutate. Produces dialog evidence with the dialog's message and the action taken. On failure with deadlineExceeded, no dialog opened in time -- confirm the triggering action actually opens one.",
        "pdf" => "Print a page to a PDF artifact with optional layout and scale. Requires browser:mutate. Produces a PDF artifact with its size and checksum. On failure with invalidRequest, scale is out of range -- pass a value between 0.1 and 2.0.",
        "cookie_set" => "Store cookies on a page's jar. Requires browser:mutate. Produces the updated cookie-jar state. On failure with invalidRequest, more than 128 cookies were passed in one call -- split into batches of 128 or fewer.",
        "cookie_delete" => "Delete cookies from a page's jar by origin and optionally by name. Requires browser:mutate. Destructive: matching cookies are removed immediately. On failure with notFound, the page id is stale -- call page_list for current ids.",
        "extract_structured" => "Extract schema-shaped JSON from a page via the configured vision provider. Requires browser:mutate and vision:assist. Produces structured-extraction evidence with the schema-shaped value. On failure with visionAssistDenied, the session's vision policy or provider isn't enabled -- read the page with inspect or a11y_snapshot instead.",
        "download_url" => "Download a URL into the session's downloads, bounded by a byte limit. Requires browser:mutate and file:download. Produces a download artifact with its size and checksum. On failure with networkPolicyDenied, use an http(s) URL without embedded credentials and a max_bytes within the configured range.",
        "upload_files" => "Set files on a file input from the runtime's configured upload roots. Requires browser:mutate and file:upload. Produces upload evidence naming the selector and resolved paths. On failure with policyDenied, the path is outside the configured upload roots -- pass a path under an allowed root.",
        "evaluate_javascript" => "Evaluate a JavaScript expression on a page, optionally awaiting its promise. Requires browser:mutate and javascript:evaluate. Produces the returned value, or notes truncation. On failure with policyDenied, the session's execution policy forbids evaluation -- use a11y_snapshot and the intent_* tools instead.",
        "command_execute" => "Execute one bounded browser command envelope naming its own capability and evidence. Requires browser:mutate, plus whatever the wrapped command needs. Produces the same evidence as the named command it wraps. On failure with deadlineOutOfRange, set the envelope's deadline within the allowed window and resubmit.",
        "intent_locate" => "Locate an element by described purpose and hints, without acting on it (Replayable). Requires browser:mutate and intent:execute. Produces resolution evidence with the matched target's fingerprint. On failure with targetNotFound or targetAmbiguous, narrow the purpose or hints and retry.",
        "intent_fill" => "Fill one described form control and verify the value (Reconciliable). Requires browser:mutate and intent:execute. Produces fill evidence carrying the browser's own validity state. On failure with verificationFailed, read the retained validation message and re-fill; on targetNotFound, take a fresh a11y_snapshot and pass the new target.",
        "intent_complete_form" => "Fill an ordered list of named form fields as one intent, verifying each before the next; never submits (Reconciliable). Requires browser:mutate and intent:execute. Produces per-field resolution and fill evidence in order. On failure with verificationFailed, targetNotFound, or intentActionMismatch on one field, the fields before it are already filled -- re-run with only the remaining fields.",
        "intent_submit_and_verify" => "Submit a form and verify the expected state (Boundary). Requires browser:mutate and intent:execute. autoCheckpoint defaults to true and mints the checkpoint in this call (returns checkpointId); pass false and pin commandId/attemptId with checkpoint_save first to author invariants. On failure with needsReconciliation, do not retry -- call recovery_status.",
        "intent_wait_for_state" => "Wait for a described page state to hold (Replayable). Requires browser:mutate and intent:execute. Produces wait evidence with elapsed time and observation count. On failure with waitConditionTimedOut, confirm the condition still matches page state via inspect, then retry with a longer timeout.",
        "intent_follow" => "Activate a described link or control and verify the destination (Boundary when boundary is true, else Reconciliable). Requires browser:mutate and intent:execute. When boundary is true, autoCheckpoint defaults true and mints the checkpoint in that call; pass false to author your own. On failure with needsReconciliation, do not retry -- call recovery_status; on targetNotFound, snapshot again.",
        "intent_dismiss_obstruction" => "Dismiss a popup, overlay, or cookie banner blocking the page (Reconciliable). Requires browser:mutate and intent:execute. Produces resolution and dismissal evidence. On failure with obstructionSuspected, the obstruction is still present after the attempt -- take a fresh a11y_snapshot to find another dismissal control.",
        "intent_extract" => "Read named fields off the page without mutating it (Replayable). Requires browser:mutate and intent:execute. Produces one extraction result per named field, with a resolution path and error code for any that failed. On failure with notFound, the session or page id is stale -- call page_list; a single unresolved field is reported per field, not as a call failure.",
        "network_log" => "Dump the page's recorded network log as a HAR artifact, then clear the buffer unless clear is false. Requires browser:mutate. Produces HAR-artifact evidence with entry count, byte size, and checksum. On failure: verificationFailed (no HAR captured), browserCommandFailed (engine could not persist it), or internal (write failed) -- none caller-fixable; retry, and report if it persists.",
        "job_submit" => "Submit a named job. Built-in handlers: echo (returns payload), sleep (payload.ms, default 1000, cap 30000). Requires job:submit. Same as POST /v1/jobs. On failure with notFound, retry submit.",
        "job_status" => "Read one owned job by id. Requires job:read. Same as GET /v1/jobs/{job}. On failure with notFound, the id is unknown or not owned.",
        "job_cancel" => "Cancel one owned job by id. Requires job:cancel. Same as DELETE /v1/jobs/{job}. On failure with notFound, the id is unknown or not owned.",
        _ => "Runtime operation.",
    }

}
