//! Capability, operation, and description tables for MCP tools.
//!
//! Host annotations live in `annotations.rs`. These maps are the enforcement
//! and agent-facing description authority for `tools/list` / `tools/call`.

/// Shared authority contracts for the callable workflow surface.
pub(crate) const WORKFLOW_START_REQUIRED_CAPABILITIES: &[types::Capability] = &[
    types::Capability::SessionRead,
    types::Capability::SessionWrite,
    types::Capability::PageWrite,
];
pub(crate) const WORKFLOW_START_OPERATION: types::InterfaceOperation =
    types::InterfaceOperation::CreateSession;

pub(crate) const WORKFLOW_OBSERVE_REQUIRED_CAPABILITIES: &[types::Capability] =
    &[types::Capability::BrowserMutate];
pub(crate) const WORKFLOW_OBSERVE_OPERATION: types::InterfaceOperation =
    types::InterfaceOperation::SubmitCommand;

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
        "workflow_start" => Some(WORKFLOW_START_REQUIRED_CAPABILITIES),
        "workflow_observe" => Some(WORKFLOW_OBSERVE_REQUIRED_CAPABILITIES),
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
        "workflow_start" => Some(WORKFLOW_START_OPERATION),
        "workflow_observe" => Some(WORKFLOW_OBSERVE_OPERATION),
        "workflow_recover" => Some(types::InterfaceOperation::RecoverWorkflow),
        "job_submit" => Some(types::InterfaceOperation::SubmitJob),
        "job_status" => Some(types::InterfaceOperation::ReadJob),
        "job_cancel" => Some(types::InterfaceOperation::CancelJob),
        _ => None,
    }
}

pub(crate) fn tool_description(name: &str) -> &'static str {
    match name {
        "context_ask" => "Resolve a described control from retained context. Requires page:read. Returns a target and confidence. On failure or after a page change, refresh with a11y_snapshot.",
        "context_neighbors" => "Show the remembered form structure around a described control: its form, sibling controls, and per-intent success counters, marked as remembered rather than live-observed. Requires context:read. Returns nothing for an unknown site or control.",
        "toolset_select" => "Narrow tools/list to one phase: explore (default), act, intent, verify, or full. Requires no capability. Emits notifications/tools/list_changed, so re-read tools/list after calling it. Hidden tools stay callable; this changes what is advertised, not what is permitted.",
        "runtime_info" => "Runtime version, granted capabilities, active session count, uptime, and credential expiry. Requires session:read.",
        "session_list" => "List browser sessions visible to this principal, each with its profile and open-page count. Requires session:read.",
        "page_list" => "List open pages in an owned session, each with its id, URL, and title. Requires browser:mutate.",
        "inspect" => "Read a page's text, optionally scoped to one element by selector or target, with HTML on request. Requires browser:mutate.",
        "a11y_snapshot" => "Capture a compact accessibility tree for a page, capped at 2048 nodes, with command-ready targets on actionable nodes. Requires browser:mutate. Start here: pass a node's target into an intent_* tool rather than guessing a selector.",
        "form_snapshot" => "Read a bounded, engine-neutral inventory of a page's form controls without exposing selectors or sensitive values. Requires page:read.",
        "screenshot" => "Capture a screenshot artifact of a page's viewport, full page, or one element. Requires browser:mutate.",
        "events_read" => "Read retained events after a cursor. Requires session:read. It blocks until a newer event or deadline; notifications/bobby/event pushes the same frames. On failure, resume from the last cursor.",
        "recovery_status" => "Read a workflow's checkpoint and recovery receipts without attempting recovery, or pass sessionId instead of workflowId to list that session's recoverable workflows newest-first. Pass exactly one. Requires recovery:read.",
        "cookie_get" => "Read cookies visible to a page, optionally filtered by URL. Requires browser:mutate.",
        "checkpoint_save" => "Persist a verified checkpoint from evidenceRefs. Requires recovery:write. Save before Boundary commands with pinned boundary IDs. On failure, confirm each referenced command completed.",
        "workflow_recover" => "Recover from the last verified checkpoint. Requires recovery:write. Returns resume, restart, or reconciliation evidence. On failure with notFound, verify session ownership with session_list.",
        "workflow_start" => "Start workflow. Requires session:read, session:write, page:write. On failure, inspect session_list.",
        "workflow_observe" => "Observe retained or live accessibility evidence. Requires browser:mutate; forms require page:read.",
        "session_create" => "Create a browser session with a profile, optional proxy, and execution policy. Requires session:write. Produces the session's id and initial state. On failure with resourceExhausted, this principal already holds its session limit -- close an idle one first.",
        "session_close" => "Close a session and release its pages, workers, and artifacts. Requires session:write. Destructive: in-flight commands on the session are cancelled. On failure, the session may already be closed -- confirm with session_list.",
        "page_open" => "Open a page, optionally navigating it. Requires page:write and browser:mutate when URL is set. Returns page and navigation state. On failure with notFound, check session_list.",
        "page_close" => "Close a page in an owned session. Requires browser:mutate. Destructive: the page and its in-flight commands are gone immediately. On failure with notFound, the page id is stale -- call page_list for current ids.",
        "page_activate" => "Bring a page to the front in an owned session. Requires browser:mutate. Produces the activated page's URL and title. On failure with notFound, the page id is stale -- call page_list for current ids.",
        "navigate" => "Navigate and wait for a load state. Requires browser:mutate. Returns settled URL/title evidence. On failure, use http(s)/data URLs or increase timeout_ms for deadlineExceeded.",
        "click" => "Click a selector or resolved target. Requires browser:mutate. Boundary clicks auto-checkpoint unless disabled. On failure with targetNotFound or targetAmbiguous, refresh a11y_snapshot.",
        "type_text" => "Type text into an element identified by a selector or a resolved target, optionally clearing it first. Requires browser:mutate. Produces execution-path evidence for the input. On failure with targetNotFound or targetAmbiguous, take a fresh a11y_snapshot and pass the new target.",
        "wait_for" => "Wait for a page condition with a bounded timeout. Requires browser:mutate. Produces wait evidence with elapsed time and observation count. On failure with waitConditionTimedOut, confirm the condition still matches page state via inspect, then retry with a longer timeout.",
        "control_action" => "Apply one native control action and reread state. Requires browser:mutate and file:upload for setFiles. On failure with targetNotFound, refresh form_snapshot.",
        "emulate" => "Set viewport, mobile, or geolocation overrides. Requires browser:mutate. Returns applied state. On failure with invalidRequest, use valid dimensions and coordinates.",
        "dialog" => "Accept or dismiss the next dialog. Requires browser:mutate. Returns message/action evidence. On failure with deadlineExceeded, verify the trigger opens a dialog.",
        "pdf" => "Print a page to a PDF artifact with optional layout and scale. Requires browser:mutate. Produces a PDF artifact with its size and checksum. On failure with invalidRequest, scale is out of range -- pass a value between 0.1 and 2.0.",
        "cookie_set" => "Store cookies on a page's jar. Requires browser:mutate. Produces the updated cookie-jar state. On failure with invalidRequest, more than 128 cookies were passed in one call -- split into batches of 128 or fewer.",
        "cookie_delete" => "Delete cookies from a page's jar by origin and optionally by name. Requires browser:mutate. Destructive: matching cookies are removed immediately. On failure with notFound, the page id is stale -- call page_list for current ids.",
        "extract_structured" => "Extract schema-shaped JSON with vision. Requires browser:mutate and vision:assist. On failure with visionAssistDenied, use inspect or a11y_snapshot.",
        "download_url" => "Download an http(s) URL within max_bytes. Requires browser:mutate and file:download. On failure with networkPolicyDenied, remove credentials and use the configured size range.",
        "upload_files" => "Set file inputs from allowed upload roots. Requires browser:mutate and file:upload. On failure with policyDenied, choose a path under a configured root.",
        "evaluate_javascript" => "Evaluate JavaScript, optionally awaiting it. Requires browser:mutate and javascript:evaluate. On failure with policyDenied, use a11y_snapshot and intent_* tools.",
        "command_execute" => "Execute a bounded command envelope. Requires browser:mutate plus wrapped-command capabilities. Prefer named tools. On failure, repair from the returned code or deadline.",
        "intent_locate" => "Locate a described target without acting (Replayable). Requires browser:mutate and intent:execute. On failure with targetNotFound or targetAmbiguous, narrow hints.",
        "intent_fill" => "Fill and verify one control (Reconciliable). Requires browser:mutate and intent:execute. On failure, read validation evidence or refresh a11y_snapshot.",
        "intent_complete_form" => "Fill ordered fields without submitting (Reconciliable). Requires browser:mutate and intent:execute. On failure, earlier fields remain filled; retry only remaining fields.",
        "intent_submit_and_verify" => "Submit and verify a form (Boundary). Requires browser:mutate and intent:execute. Auto-checkpoints by default. On failure with needsReconciliation, call recovery_status; do not retry.",
        "intent_wait_for_state" => "Wait for a described state (Replayable). Requires browser:mutate and intent:execute. On failure with waitConditionTimedOut, inspect state or increase timeout.",
        "intent_follow" => "Activate and verify a destination (Boundary when flagged). Requires browser:mutate and intent:execute. Boundary calls auto-checkpoint. On failure with needsReconciliation, do not retry; call recovery_status. For targetNotFound, refresh targets.",
        "intent_dismiss_obstruction" => "Dismiss a blocking overlay (Reconciliable). Requires browser:mutate and intent:execute. On failure with obstructionSuspected, refresh a11y_snapshot.",
        "intent_extract" => "Read named fields without mutation (Replayable). Requires browser:mutate and intent:execute. On failure with notFound, refresh page_list; unresolved fields report individually.",
        "network_log" => "Export and optionally clear the HAR buffer. Requires browser:mutate. Returns entry, byte, and checksum evidence. On failure, retry once and report persistent runtime errors.",
        "job_submit" => "Submit a named job. Built-in handlers: echo (returns payload), sleep (payload.ms, default 1000, cap 30000), http_probe (payload.url, method HEAD|GET, timeoutMs). Requires job:submit. Same as POST /v1/jobs. On failure with notFound, retry submit.",
        "job_status" => "Read one owned job by id. Requires job:read. Same as GET /v1/jobs/{job}. On failure with notFound, the id is unknown or not owned.",
        "job_cancel" => "Cancel one owned job by id. Requires job:cancel. Same as DELETE /v1/jobs/{job}. On failure with notFound, the id is unknown or not owned.",
        _ => "Runtime operation.",
    }
}
