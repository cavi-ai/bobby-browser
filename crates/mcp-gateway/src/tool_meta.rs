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
        "command_execute"
        | "control_action"
        | "navigate"
        | "click"
        | "click_and_wait_for_popup"
        | "type_text"
        | "inspect"
        | "screenshot"
        | "wait_for"
        | "page_list"
        | "page_close"
        | "page_activate"
        | "a11y_snapshot"
        | "pdf"
        | "dialog"
        | "emulate"
        | "network_log"
        | "cookie_get"
        | "cookie_set"
        | "cookie_delete" => Some(&[types::Capability::BrowserMutate]),
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
        // The challenge intents need the vision provider by construction: a
        // principal without vision:assist can never complete them, so the
        // gate carries it up front instead of failing at the engine after
        // the screenshot. Declaring it here also keeps the tools out of a
        // tools/list response the principal could never serve.
        "intent_detect_challenge" | "intent_solve_challenge" => Some(&[
            types::Capability::BrowserMutate,
            types::Capability::IntentExecute,
            types::Capability::VisionAssist,
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
        | "click_and_wait_for_popup"
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
        "runtime_info" => "Runtime version, feature flags, active session count, uptime, and credential expiry. Requires session:read. The capabilities list names configured features: vision-assist and vision-provider appear only when vision is wired, so check them before vision-dependent tools -- visionAssistFailed with no provider configured never succeeds on retry.",
        "session_list" => "List browser sessions visible to this principal, each with its profile and open-page count. Requires session:read.",
        "page_list" => "List open pages in an owned session, each with its id, URL, and title. Requires browser:mutate.",
        "inspect" => "Read a page's visible text, optionally scoped to one element by selector or target (a scoped control returns its value), with HTML on request. Whole-page reads do not include form field values -- use form_snapshot for those. Requires browser:mutate.",
        "a11y_snapshot" => "Capture a compact accessibility tree with command-ready targets, capped at 2048 nodes. Pass target to scope it to one form or dialog. Same-process iframe targets include their frame hop; use the in-frame target directly. Links include their URL. Requires browser:mutate.",
        "form_snapshot" => "Read a bounded, engine-neutral inventory of a page's form controls with each control's current state: text values, checked, selection, file count (passwords redacted, no selectors). Requires page:read. Use it to verify what a fill or submit actually landed.",
        "screenshot" => "Capture a screenshot artifact of a page's viewport, full page, or one element. Requires browser:mutate.",
        "events_read" => "Read retained events after a cursor. Requires session:read. It blocks until a newer event or deadline; notifications/bobby/event pushes the same frames. On failure, resume from the last cursor.",
        "recovery_status" => "Read a workflow's checkpoint and recovery receipts without attempting recovery, or pass sessionId instead of workflowId to list that session's recoverable workflows newest-first. Pass exactly one. Requires recovery:read.",
        "cookie_get" => "Read cookies visible to a page, optionally filtered by URL. Requires browser:mutate.",
        "checkpoint_save" => "Persist a verified checkpoint from evidenceRefs. Requires recovery:write. Save before Boundary commands with pinned boundary IDs. On failure, confirm each referenced command completed.",
        "workflow_recover" => "Recover from the last verified checkpoint. Requires recovery:write. Returns resume, restart, or reconciliation evidence. On failure with notFound, verify session ownership with session_list.",
        "workflow_start" => "Create and bind a browser session, page, and retained workflow, optionally navigating to url. This is the canonical first browser call. Requires session:read, session:write, page:write. On failure, inspect session_list.",
        "workflow_observe" => "Observe retained or live accessibility evidence. Requires browser:mutate; forms require page:read. Pass target role=main to omit repeated site chrome. Live observations default evidenceDetail=compact; pass full for diagnostics.",
        "session_create" => "Create a browser session with a profile, optional proxy, and execution policy. Requires session:write. Produces the session's id and initial state. On failure with resourceExhausted, this principal already holds its session limit -- close an idle one first.",
        "session_close" => "Close a session and release its pages, workers, and artifacts. Requires session:write. Destructive: in-flight commands on the session are cancelled. On failure, the session may already be closed -- confirm with session_list.",
        "page_open" => "Open a page, optionally navigating it. Requires page:write and browser:mutate when URL is set. Returns page and navigation state. On failure with notFound, check session_list.",
        "page_close" => "Close a page in an owned session. Requires browser:mutate. Destructive: the page and its in-flight commands are gone immediately. On failure with notFound, the page id is stale -- call page_list for current ids.",
        "page_activate" => "Bring a page to the front in an owned session. Requires browser:mutate. Produces the activated page's URL and title. On failure with notFound, the page id is stale -- call page_list for current ids.",
        "navigate" => "Navigate and wait for a load state. Requires browser:mutate. Returns settled URL/title evidence. On failure, use http(s)/data URLs or increase timeout_ms for deadlineExceeded.",
        "click" => "Click a selector or resolved target with optional Shift, Ctrl, Alt, or Meta modifiers. Requires browser:mutate. Boundary clicks auto-checkpoint unless disabled. On failure with targetNotFound or targetAmbiguous, refresh a11y_snapshot.",
        "click_and_wait_for_popup" => "Click a control and wait for a window.open popup to register in page_list (Boundary). Requires browser:mutate. autoCheckpoint defaults true (minted in this call); pass false and pin commandId/attemptId with checkpoint_save to author invariants. Prefer over plain click for popups. On failure with deadlineExceeded, confirm the click opens a window; on needsReconciliation, call recovery_status.",
        "type_text" => "Type text into an element identified by a selector or a resolved target, optionally clearing it first. Requires browser:mutate. Produces execution-path evidence for the input. On failure with targetNotFound or targetAmbiguous, take a fresh a11y_snapshot and pass the new target.",
        "wait_for" => "Wait for a page condition with a bounded timeout. Requires browser:mutate. Produces wait evidence with elapsed time and observation count. On failure with waitConditionTimedOut, confirm the condition still matches page state via inspect, then retry with a longer timeout.",
        "control_action" => "Apply one native control action and reread state. Requires browser:mutate and file:upload for setFiles. Pass a snapshot target verbatim: role + accessibleName suffice; ordinal and frame/shadow hops default to empty. selectOne and selectMany accept an option's value or its visible label. On failure with targetNotFound, refresh form_snapshot.",
        "emulate" => "Set viewport, mobile, or geolocation overrides. Requires browser:mutate. Returns applied state. On failure with invalidRequest, use valid dimensions and coordinates.",
        "dialog" => "Accept or dismiss the next dialog. Requires browser:mutate. Returns message/action evidence. On failure with deadlineExceeded, verify the trigger opens a dialog.",
        "pdf" => "Print a page to a PDF artifact with optional layout and scale. Requires browser:mutate. Produces a PDF artifact with its size and checksum. On failure with invalidRequest, scale is out of range -- pass a value between 0.1 and 2.0.",
        "cookie_set" => "Store cookies on a page's jar. Requires browser:mutate. Produces the updated cookie-jar state. On failure with invalidRequest, more than 128 cookies were passed in one call -- split into batches of 128 or fewer.",
        "cookie_delete" => "Delete cookies from a page's jar by origin and optionally by name. Requires browser:mutate. Destructive: matching cookies are removed immediately. On failure with notFound, the page id is stale -- call page_list for current ids.",
        "extract_structured" => "Extract schema-shaped JSON from a page via the configured vision provider. Requires browser:mutate and vision:assist. Produces structured-extraction evidence with the schema-shaped value. On failure with visionAssistDenied, the session's vision policy or provider isn't enabled -- read the page with inspect or a11y_snapshot instead.",
        "download_url" => "Download URL within maxBytes. Requires browser:mutate and file:download. The advertised maximum is configured. Pass absolute saveAs; savedTo echoes it and sha256 proves integrity, so no shell check is needed. Escapes/overwrites fail pre-fetch. On failure, obey the maximum or repair URL policy.",
        "upload_files" => "Set files on a file input from the runtime's configured upload roots. Accepts selector, target, or a controlId returned by form_snapshot. Requires browser:mutate and file:upload. Produces upload evidence naming the selector and resolved paths. On failure with policyDenied, the path is outside the configured upload roots -- pass a path under an allowed root.",
        "evaluate_javascript" => "Evaluate a JavaScript expression on a page, optionally awaiting its promise. Requires browser:mutate and javascript:evaluate. Produces the returned value, or notes truncation. On failure with policyDenied, the session's execution policy forbids evaluation -- use a11y_snapshot and the intent_* tools instead.",
        "command_execute" => "Execute one bounded browser command envelope naming its own capability and evidence. Requires browser:mutate, plus whatever the wrapped command needs. Produces the same evidence as the named command it wraps. On failure with deadlineOutOfRange, set the envelope's deadline within the allowed window and resubmit.",
        "intent_locate" => "Locate an element by described purpose and hints, without acting on it (Replayable). Requires browser:mutate and intent:execute. Produces resolution evidence with the matched target's fingerprint. On failure with targetNotFound or targetAmbiguous, narrow the purpose or hints and retry.",
        "intent_fill" => "Fill one described form control and verify the value (Reconciliable). Requires browser:mutate and intent:execute. Produces fill evidence carrying the browser's own validity state. On failure with verificationFailed, read the retained validation message and re-fill; on targetNotFound, take a fresh a11y_snapshot and pass the new target.",
        "intent_complete_form" => "Fill ordered named fields in one verified intent; never submits. Requires browser:mutate and intent:execute. Prefer over repeated intent_fill calls. Fields resolve just-in-time: include conditional fields after their revealer even if absent. Success defaults evidenceDetail=compact. On failure, prior fields remain filled; retry only remaining fields.",
        "intent_submit_and_verify" => "Submit once and verify post-state. Requires browser:mutate and intent:execute. Unknown copy: networkQuiet returns inspection and submitSettlement=settled|validationRejected; rejection includes formValidation issues, so do not inspect or blindly resubmit. Use text/url for known success. On failure with needsReconciliation call recovery_status.",
        "intent_wait_for_state" => "Wait for a described page state to hold (Replayable). Requires browser:mutate and intent:execute. Produces wait evidence with elapsed time and observation count. On failure with waitConditionTimedOut, confirm the condition still matches page state via inspect, then retry with a longer timeout.",
        "intent_follow" => "Activate a described link or control and verify the destination (Boundary when boundary is true, else Reconciliable). Requires browser:mutate and intent:execute. When boundary is true, autoCheckpoint defaults true and mints the checkpoint in that call; pass false to author your own. On failure with needsReconciliation, do not retry -- call recovery_status; on targetNotFound, snapshot again.",
        "intent_dismiss_obstruction" => "Dismiss a popup, overlay, or cookie banner blocking the page (Reconciliable). Requires browser:mutate and intent:execute. Produces resolution and dismissal evidence. On failure with obstructionSuspected, the obstruction is still present after the attempt -- take a fresh a11y_snapshot to find another dismissal control.",
        "intent_extract" => "Read named fields off the page without mutating it (Replayable). Requires browser:mutate and intent:execute. Produces one extraction result per named field, with a resolution path and error code for any that failed. On failure with notFound, the session or page id is stale -- call page_list; a single unresolved field is reported per field, not as a call failure.",
        "intent_detect_challenge" => "Classify a captcha or verification challenge without acting (Replayable). Requires browser:mutate, intent:execute, and vision:assist (plus visionAssist policy). Produces challengeDetection evidence: type, confidence, blocking; a clean page is a first-class answer. visionAssistDenied: enable the session policy first; else snapshot after the page changes.",
        "intent_solve_challenge" => "Drive the vision solve loop until the challenge is cleared or timeoutMs elapses (Reconciliable). Requires browser:mutate, intent:execute, and vision:assist (plus visionAssist policy). Detect first when the kind is unclear. visionAssistFailed: the provider misread the challenge — retry once, then surface to the operator; the runtime never bypasses a challenge.",
        "network_log" => "Dump the page's recorded network log as a HAR artifact, then clear the buffer unless clear is false. Recording starts at the first call on a page (it reports networkRecordingStarted): call it once before the traffic you want. Requires browser:mutate. Produces HAR-artifact evidence (entries, bytes, checksum). On failure (verificationFailed, browserCommandFailed, internal) retry; not caller-fixable.",
        "job_submit" => "Submit a built-in job: echo; sleep (ms, default 1000, cap 30000); http_probe (url, HEAD|GET, timeoutMs); http_wait (url, timeoutMs/intervalMs/probeTimeoutMs, contains); or http_fetch (url, GET, contains/maxBodyBytes/timeoutMs). Requires job:submit. Same as POST /v1/jobs. On failure after submission, use job_status on the returned id; unknown names fail asynchronously, so do not resubmit.",
        "job_status" => "Read one owned job by id. Requires job:read. Same as GET /v1/jobs/{job}. On failure with notFound, the id is unknown or not owned.",
        "job_cancel" => "Cancel one owned job by id. Requires job:cancel. Same as DELETE /v1/jobs/{job}. On failure with notFound, the id is unknown or not owned.",
        _ => "Runtime operation.",
    }
}

#[cfg(test)]
mod tests {
    use super::tool_description;

    #[test]
    fn form_intents_describe_the_compact_verified_loop() {
        let complete = tool_description("intent_complete_form");
        assert!(
            complete.contains("Prefer over repeated intent_fill calls"),
            "whole-form intent must advertise its round-trip advantage"
        );
        assert!(
            complete.contains("evidenceDetail=compact"),
            "whole-form intent must advertise compact success evidence"
        );
        assert!(
            complete.contains("just-in-time")
                && complete.contains("conditional fields")
                && complete.contains("absent"),
            "whole-form intent must explain that ordered conditional fields resolve against fresh page state"
        );

        let submit = tool_description("intent_submit_and_verify");
        assert!(
            submit.contains("Submit once")
                && submit.contains("submitSettlement=settled|validationRejected")
                && submit.contains("do not inspect or blindly resubmit"),
            "verified submit must identify the exactly-once settled stopping condition"
        );
    }

    #[test]
    fn iframe_and_download_descriptions_identify_terminal_evidence() {
        let snapshot = tool_description("a11y_snapshot");
        assert!(
            snapshot.contains("use the in-frame target directly"),
            "iframe targets must discourage a redundant second discovery pass"
        );

        let download = tool_description("download_url");
        assert!(
            download.contains("no shell check is needed"),
            "digest-verified saved downloads must identify terminal evidence"
        );
    }
}
