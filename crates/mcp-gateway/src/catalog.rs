//! Canonical tool descriptors. Enforcement, advertisement, schemas, retained
//! scope, and routing are derived from this table; handlers own only execution.

#[cfg(test)]
use crate::tool_args::*;
use crate::workflow_handles::WorkflowScope;
#[cfg(test)]
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

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

pub(crate) const ALWAYS: u8 = 1;
pub(crate) const EXPLORE: u8 = 2;
pub(crate) const ACT: u8 = 4;
pub(crate) const INTENT: u8 = 8;
pub(crate) const VERIFY: u8 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DispatchGroup {
    Lifecycle,
    AgentWorkflow,
    Primitives,
    Intents,
    PageOps,
    Workflow,
    Toolset,
}

pub(crate) struct ToolDescriptor {
    pub catalog_order: usize,
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub capabilities: &'static [types::Capability],
    pub operation: Option<types::InterfaceOperation>,
    pub schema: fn() -> (Value, Vec<&'static str>),
    #[cfg(test)]
    pub parse: fn(Value) -> Result<(), ()>,
    pub phases: u8,
    pub scope: Option<WorkflowScope>,
    pub group: DispatchGroup,
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
    pub open_world: bool,
    pub page_derived: bool,
}

#[cfg(test)]
fn parse<T: DeserializeOwned>(arguments: Value) -> Result<(), ()> {
    serde_json::from_value::<T>(arguments)
        .map(drop)
        .map_err(|_| ())
}

macro_rules! tools {
    ($( $name:ident { order: $order:literal, title: $title:expr, description: $description:expr,
        capabilities: $caps:expr, operation: $operation:expr,
        schema: $schema:ident, args: $args:ty, phases: $phases:expr,
        $(scope: $scope:ident,)? group: $group:ident,
        $(page_derived: $derived:literal,)?
        hints: ($read:expr, $destructive:expr, $idempotent:expr, $open:expr) } )*) => {
        pub const EVERY_TOOL: &[&str] = &[$(stringify!($name)),*];
        #[cfg(test)]
        pub(crate) const WORKFLOW_SCOPE_TOOLS: &[(&str, WorkflowScope)] = &[
            $($((stringify!($name), WorkflowScope::$scope),)?)*
        ];
        pub(crate) static DESCRIPTORS: &[ToolDescriptor] = &[$(ToolDescriptor {
            catalog_order: $order, name: stringify!($name), title: $title, description: $description,
            capabilities: $caps, operation: $operation,
            schema: crate::schema::$schema, #[cfg(test)] parse: parse::<$args>, phases: $phases,
            scope: tools!(@scope $($scope)?), group: DispatchGroup::$group,
            read_only: $read, destructive: $destructive, idempotent: $idempotent, open_world: $open,
            page_derived: tools!(@flag $($derived)?),
        }),*];
    };
    (@scope $scope:ident) => { Some(WorkflowScope::$scope) };
    (@scope) => { None };
    (@flag $derived:literal) => { $derived };
    (@flag) => { false };
}

pub(crate) fn descriptor(name: &str) -> Option<&'static ToolDescriptor> {
    DESCRIPTORS
        .binary_search_by_key(&name, |tool| tool.name)
        .ok()
        .map(|index| &DESCRIPTORS[index])
}

pub(crate) fn required_capabilities(name: &str) -> Option<&'static [types::Capability]> {
    descriptor(name).map(|tool| tool.capabilities)
}

pub(crate) fn required_operation(name: &str) -> Option<types::InterfaceOperation> {
    descriptor(name).and_then(|tool| tool.operation)
}

pub(crate) fn tool_description(name: &str) -> &'static str {
    descriptor(name).map_or("Runtime operation.", |tool| tool.description)
}

pub(crate) fn tool_title(name: &str) -> &'static str {
    descriptor(name).map_or("Untitled tool", |tool| tool.title)
}

pub(crate) fn is_page_derived(name: &str) -> bool {
    descriptor(name).is_some_and(|tool| tool.page_derived)
}

/// MCP host hints. `required_capabilities` remains the authority over what a
/// principal may call.
pub(crate) fn tool_annotations(name: &str) -> Value {
    let tool = descriptor(name);
    let read_only = tool.is_some_and(|tool| tool.read_only);
    let destructive = tool.is_some_and(|tool| tool.destructive);
    let idempotent = tool.is_some_and(|tool| tool.idempotent);
    let open_world = tool.is_some_and(|tool| tool.open_world);
    let mut hints = serde_json::Map::new();
    if read_only {
        hints.insert("readOnlyHint".to_owned(), json!(true));
    } else {
        hints.insert("destructiveHint".to_owned(), json!(destructive));
    }
    if idempotent {
        hints.insert("idempotentHint".to_owned(), json!(true));
    }
    hints.insert("openWorldHint".to_owned(), json!(open_world));
    Value::Object(hints)
}

tools! {
    a11y_snapshot { order: 32, title: "Accessibility snapshot", description: "Capture a compact accessibility tree with command-ready targets. Same-process iframe targets include their frame hop; use the in-frame target directly. Requires browser:mutate.",
        capabilities: &[types::Capability::BrowserMutate], operation: Some(types::InterfaceOperation::SubmitCommand), schema: a11y_snapshot_schema, args: A11ySnapshotArgs,
        phases: EXPLORE | ACT | INTENT | VERIFY, scope: SessionPageWorkflow, group: PageOps, page_derived: true, hints: (true, false, false, false) }
    checkpoint_save { order: 0, title: "Save checkpoint", description: "Persist a verified checkpoint from evidenceRefs. Requires recovery:write. Save before Boundary commands with pinned boundary IDs. On failure, confirm each referenced command completed.",
        capabilities: &[types::Capability::RecoveryWrite], operation: Some(types::InterfaceOperation::CreateCheckpoint), schema: checkpoint_save_schema, args: CheckpointSaveArgs,
        phases: INTENT | VERIFY, group: Workflow, hints: (false, false, true, false) }
    click { order: 1, title: "Click", description: "Click a selector or resolved target; pass workflowHandle, optional Shift/Ctrl/Alt/Meta modifiers. Requires browser:mutate. On failure with targetNotFound or targetAmbiguous, refresh a11y_snapshot.",
        capabilities: &[types::Capability::BrowserMutate], operation: Some(types::InterfaceOperation::SubmitCommand), schema: click_schema, args: ClickArgs,
        phases: EXPLORE | ACT, scope: SessionPageWorkflow, group: Primitives, hints: (false, false, false, true) }
    click_and_wait_for_download { order: 2, title: "Click and wait for download", description: "Click a control and wait for its browser download to finish (Boundary). Requires browser:mutate and file:download. On failure with needsReconciliation, call recovery_status.",
        capabilities: &[
            types::Capability::BrowserMutate,
            types::Capability::FileDownload,
        ], operation: Some(types::InterfaceOperation::SubmitCommand), schema: click_and_wait_for_download_schema, args: ClickAndWaitForDownloadArgs,
        phases: ACT, scope: SessionPageWorkflow, group: Primitives, hints: (false, false, false, true) }
    click_and_wait_for_popup { order: 3, title: "Click and wait for popup", description: "Click a control and wait for a window.open popup to register in page_list (Boundary). Requires browser:mutate. On failure with needsReconciliation, call recovery_status.",
        capabilities: &[types::Capability::BrowserMutate], operation: Some(types::InterfaceOperation::SubmitCommand), schema: click_and_wait_for_download_schema, args: ClickAndWaitForPopupArgs,
        phases: EXPLORE | ACT, scope: SessionPageWorkflow, group: Primitives, hints: (false, false, false, false) }
    command_execute { order: 9, title: "Execute command envelope", description: "Execute one bounded browser command envelope naming its own capability and evidence. Requires browser:mutate, plus whatever the wrapped command needs. Produces the same evidence as the named command it wraps. On failure with deadlineOutOfRange, set the envelope's deadline within the allowed window and resubmit.",
        capabilities: &[types::Capability::BrowserMutate], operation: Some(types::InterfaceOperation::SubmitCommand), schema: command_execute_schema, args: CommandExecuteArgs,
        phases: ACT, group: Primitives, hints: (false, false, false, true) }
    context_ask { order: 4, title: "Ask where a control is", description: "Resolve a described control from retained context. Requires page:read. Returns a target and confidence. On failure or after a page change, refresh with a11y_snapshot.",
        capabilities: &[types::Capability::PageRead], operation: Some(types::InterfaceOperation::ReadPage), schema: context_ask_schema, args: ContextAskArgs,
        phases: ACT | INTENT | VERIFY, scope: SessionPage, group: PageOps, page_derived: true, hints: (true, false, false, false) }
    context_neighbors { order: 5, title: "Show remembered form structure around a control", description: "Show the remembered form structure around a described control: its form, sibling controls, and per-intent success counters, marked as remembered rather than live-observed. Requires context:read. Returns nothing for an unknown site or control.",
        capabilities: &[types::Capability::ContextRead], operation: Some(types::InterfaceOperation::ReadContext), schema: context_ask_schema, args: ContextNeighborsArgs,
        phases: INTENT | VERIFY, scope: SessionPage, group: PageOps, page_derived: true, hints: (true, false, false, false) }
    control_action { order: 10, title: "Form control action", description: "Apply one native control action and reread state. Pass a snapshot target verbatim. Requires browser:mutate; file:upload for setFiles. On failure with targetNotFound, refresh form_snapshot.",
        capabilities: &[types::Capability::BrowserMutate], operation: Some(types::InterfaceOperation::SubmitCommand), schema: control_action_schema, args: ControlActionArgs,
        phases: EXPLORE | ACT, scope: SessionPageWorkflow, group: PageOps, hints: (false, false, false, false) }
    cookie_delete { order: 6, title: "Delete cookies", description: "Delete cookies from a page's jar by origin and optionally by name. Requires browser:mutate. Destructive: matching cookies are removed immediately. On failure with notFound, the page id is stale -- call page_list for current ids.",
        capabilities: &[types::Capability::BrowserMutate], operation: Some(types::InterfaceOperation::SubmitCommand), schema: cookie_delete_schema, args: CookieDeleteArgs,
        phases: ACT, scope: SessionPageWorkflow, group: PageOps, hints: (false, true, false, false) }
    cookie_get { order: 7, title: "Read cookies", description: "Read cookies visible to a page, optionally filtered by URL. Requires browser:mutate.",
        capabilities: &[types::Capability::BrowserMutate], operation: Some(types::InterfaceOperation::SubmitCommand), schema: cookie_get_schema, args: CookieGetArgs,
        phases: ACT, scope: SessionPageWorkflow, group: PageOps, hints: (true, false, false, false) }
    cookie_set { order: 8, title: "Set cookies", description: "Store cookies on a page's jar. Requires browser:mutate. Produces the updated cookie-jar state. On failure with invalidRequest, more than 128 cookies were passed in one call -- split into batches of 128 or fewer.",
        capabilities: &[types::Capability::BrowserMutate], operation: Some(types::InterfaceOperation::SubmitCommand), schema: cookie_set_schema, args: CookieSetArgs,
        phases: ACT, scope: SessionPageWorkflow, group: PageOps, hints: (false, false, false, false) }
    dialog { order: 21, title: "Handle dialog", description: "Accept or dismiss the next dialog. Requires browser:mutate. Returns message/action evidence. On failure with deadlineExceeded, verify the trigger opens a dialog.",
        capabilities: &[types::Capability::BrowserMutate], operation: Some(types::InterfaceOperation::SubmitCommand), schema: dialog_schema, args: DialogArgs,
        phases: ACT, scope: SessionPageWorkflow, group: PageOps, hints: (false, false, false, false) }
    download_url { order: 22, title: "Download URL", description: "Download URL within maxBytes. Requires browser:mutate and file:download. The advertised maximum is configured. Pass absolute saveAs; savedTo echoes it and sha256 proves integrity, so no shell check is needed. Escapes/overwrites fail pre-fetch. On failure, obey the maximum or repair URL policy.",
        capabilities: &[
            types::Capability::BrowserMutate,
            types::Capability::FileDownload,
        ], operation: Some(types::InterfaceOperation::SubmitCommand), schema: download_url_schema, args: DownloadUrlArgs,
        phases: ACT, scope: SessionPageWorkflow, group: PageOps, hints: (false, false, false, true) }
    emulate { order: 23, title: "Emulate device", description: "Set viewport, mobile, or geolocation overrides. Requires browser:mutate. Returns applied state. On failure with invalidRequest, use valid dimensions and coordinates.",
        capabilities: &[types::Capability::BrowserMutate], operation: Some(types::InterfaceOperation::SubmitCommand), schema: emulate_schema, args: EmulateArgs,
        phases: ACT, scope: SessionPageWorkflow, group: PageOps, hints: (false, false, true, false) }
    evaluate_javascript { order: 24, title: "Evaluate JavaScript", description: "Evaluate a JavaScript expression on a page, optionally awaiting its promise. Requires browser:mutate and javascript:evaluate. Produces the returned value, or notes truncation. On failure with policyDenied, the session's execution policy forbids evaluation -- use a11y_snapshot and the intent_* tools instead.",
        capabilities: &[
            types::Capability::BrowserMutate,
            types::Capability::JavascriptEvaluate,
        ], operation: Some(types::InterfaceOperation::SubmitCommand), schema: evaluate_javascript_schema, args: EvaluateJavaScriptArgs,
        phases: ACT, scope: SessionPageWorkflow, group: PageOps, hints: (false, false, false, false) }
    events_read { order: 25, title: "Read events", description: "Read retained events after a cursor. Requires session:read. It blocks until a newer event or deadline; notifications/bobby/event pushes the same frames. On failure, resume from the last cursor.",
        capabilities: &[types::Capability::SessionRead], operation: Some(types::InterfaceOperation::SubscribeEvents), schema: events_read_schema, args: EventsReadArgs,
        phases: VERIFY, group: Workflow, hints: (true, false, false, false) }
    extract_structured { order: 33, title: "Extract structured data", description: "Extract schema-shaped JSON from a page via the configured vision provider. Requires browser:mutate and vision:assist. Produces structured-extraction evidence with the schema-shaped value. On failure with visionAssistDenied, the session's vision policy or provider isn't enabled -- read the page with inspect or a11y_snapshot instead.",
        capabilities: &[
            types::Capability::BrowserMutate,
            types::Capability::VisionAssist,
        ], operation: Some(types::InterfaceOperation::SubmitCommand), schema: extract_structured_schema, args: ExtractStructuredArgs,
        phases: INTENT, scope: SessionPageWorkflow, group: PageOps, page_derived: true, hints: (false, false, false, true) }
    form_snapshot { order: 34, title: "Form snapshot", description: "Read a bounded inventory of a page's form controls and each one's current state (passwords redacted, no selectors). Requires page:read.",
        capabilities: &[types::Capability::PageRead], operation: Some(types::InterfaceOperation::ReadPage), schema: form_snapshot_input_schema, args: FormSnapshotArgs,
        phases: INTENT | VERIFY, scope: SessionPage, group: PageOps, page_derived: true, hints: (true, false, false, false) }
    inspect { order: 26, title: "Inspect page", description: "Read a page's visible text, optionally scoped to one element by selector or target, with HTML on request. Requires browser:mutate.",
        capabilities: &[types::Capability::BrowserMutate], operation: Some(types::InterfaceOperation::SubmitCommand), schema: inspect_schema, args: InspectArgs,
        phases: VERIFY, scope: SessionPageWorkflow, group: Primitives, page_derived: true, hints: (true, false, false, false) }
    intent_complete_form { order: 11, title: "Complete form", description: "Fill ordered named fields in one verified intent; never submits. Pass workflowHandle. Requires browser:mutate and intent:execute. Fields resolve just-in-time; revealedBy activates first. Defaults evidenceDetail=compact. On failure retry remaining fields.",
        capabilities: &[
            types::Capability::BrowserMutate,
            types::Capability::IntentExecute,
        ], operation: Some(types::InterfaceOperation::SubmitCommand), schema: intent_complete_form_schema, args: IntentCompleteFormArgs,
        phases: EXPLORE | INTENT, scope: SessionPageWorkflow, group: Intents, hints: (false, false, false, false) }
    intent_detect_challenge { order: 12, title: "Detect challenge", description: "Classify a captcha or verification challenge without acting (Replayable). Requires browser:mutate, intent:execute, and vision:assist (plus visionAssist policy). Produces challengeDetection evidence: type, confidence, blocking; a clean page is a first-class answer. visionAssistDenied: enable the session policy first; else snapshot after the page changes.",
        capabilities: &[
            types::Capability::BrowserMutate,
            types::Capability::IntentExecute,
            types::Capability::VisionAssist,
        ], operation: None, schema: intent_detect_challenge_schema, args: IntentDetectChallengeArgs,
        phases: INTENT, scope: SessionPageWorkflow, group: Intents, hints: (true, false, false, false) }
    intent_dismiss_obstruction { order: 13, title: "Dismiss obstruction", description: "Dismiss a popup, overlay, or cookie banner blocking the page (Reconciliable). Requires browser:mutate and intent:execute. Produces resolution and dismissal evidence. On failure with obstructionSuspected, the obstruction is still present after the attempt -- take a fresh a11y_snapshot to find another dismissal control.",
        capabilities: &[
            types::Capability::BrowserMutate,
            types::Capability::IntentExecute,
        ], operation: Some(types::InterfaceOperation::SubmitCommand), schema: intent_dismiss_obstruction_schema, args: IntentDismissObstructionArgs,
        phases: INTENT, scope: SessionPageWorkflow, group: Intents, hints: (false, false, false, false) }
    intent_extract { order: 14, title: "Extract fields", description: "Read named fields off the page without mutating it (Replayable). Requires browser:mutate and intent:execute. Produces one extraction result per named field, with a resolution path and error code for any that failed. On failure with notFound, the session or page id is stale -- call page_list; a single unresolved field is reported per field, not as a call failure.",
        capabilities: &[
            types::Capability::BrowserMutate,
            types::Capability::IntentExecute,
        ], operation: Some(types::InterfaceOperation::SubmitCommand), schema: intent_extract_schema, args: IntentExtractArgs,
        phases: INTENT, scope: SessionPageWorkflow, group: Intents, page_derived: true, hints: (true, false, false, false) }
    intent_fill { order: 15, title: "Fill control", description: "Fill one described form control and verify the value (Reconciliable). Requires browser:mutate and intent:execute. accessibleName may be a controlId from form_snapshot. Produces fill evidence carrying the browser's own validity state. On failure with verificationFailed, read the retained validation message and re-fill; on targetNotFound, take a fresh a11y_snapshot and pass the new target.",
        capabilities: &[
            types::Capability::BrowserMutate,
            types::Capability::IntentExecute,
        ], operation: Some(types::InterfaceOperation::SubmitCommand), schema: intent_fill_schema, args: IntentFillArgs,
        phases: INTENT, scope: SessionPageWorkflow, group: Intents, hints: (false, false, false, false) }
    intent_follow { order: 16, title: "Follow link", description: "Activate and verify a described control. Prefer over click plus wait_for. Requires browser:mutate and intent:execute. Defaults evidenceDetail=compact. On failure with needsReconciliation, do not retry; call recovery_status.",
        capabilities: &[
            types::Capability::BrowserMutate,
            types::Capability::IntentExecute,
        ], operation: Some(types::InterfaceOperation::SubmitCommand), schema: intent_follow_schema, args: IntentFollowArgs,
        phases: EXPLORE | INTENT, scope: SessionPageWorkflow, group: Intents, hints: (false, true, false, true) }
    intent_locate { order: 17, title: "Locate element", description: "Locate an element by described purpose and hints, without acting on it (Replayable). Requires browser:mutate and intent:execute. Produces resolution evidence with the matched target's fingerprint. On failure with targetNotFound or targetAmbiguous, narrow the purpose or hints and retry.",
        capabilities: &[
            types::Capability::BrowserMutate,
            types::Capability::IntentExecute,
        ], operation: Some(types::InterfaceOperation::SubmitCommand), schema: intent_locate_schema, args: IntentLocateArgs,
        phases: INTENT, scope: SessionPageWorkflow, group: Intents, hints: (true, false, false, false) }
    intent_solve_challenge { order: 18, title: "Solve challenge", description: "Drive the vision solve loop until the challenge is cleared or timeoutMs elapses (Reconciliable). Requires browser:mutate, intent:execute, and vision:assist (plus visionAssist policy). Detect first when the kind is unclear. On failure with visionAssistFailed, retry once, then surface to the operator; the runtime never bypasses a challenge.",
        capabilities: &[
            types::Capability::BrowserMutate,
            types::Capability::IntentExecute,
            types::Capability::VisionAssist,
        ], operation: None, schema: intent_detect_challenge_schema, args: IntentSolveChallengeArgs,
        phases: INTENT, scope: SessionPageWorkflow, group: Intents, hints: (false, false, false, false) }
    intent_submit_and_verify { order: 19, title: "Submit and verify", description: "Submit once and verify post-state. Pass workflowHandle. Requires browser:mutate and intent:execute. networkQuiet returns submitSettlement=settled|validationRejected; on rejection, do not inspect or blindly resubmit. On failure with needsReconciliation, call recovery_status.",
        capabilities: &[
            types::Capability::BrowserMutate,
            types::Capability::IntentExecute,
        ], operation: Some(types::InterfaceOperation::SubmitCommand), schema: intent_submit_and_verify_schema, args: IntentSubmitAndVerifyArgs,
        phases: EXPLORE | INTENT, scope: SessionPageWorkflow, group: Intents, hints: (false, true, false, true) }
    intent_wait_for_state { order: 20, title: "Wait for state", description: "Wait for a described page state to hold (Replayable). Requires browser:mutate and intent:execute. On failure with waitConditionTimedOut, retry with a longer timeout.",
        capabilities: &[
            types::Capability::BrowserMutate,
            types::Capability::IntentExecute,
        ], operation: Some(types::InterfaceOperation::SubmitCommand), schema: intent_wait_for_state_schema, args: IntentWaitForStateArgs,
        phases: INTENT, scope: SessionPageWorkflow, group: Intents, hints: (true, false, false, false) }
    job_cancel { order: 27, title: "Cancel job", description: "Cancel one owned job by id. Requires job:cancel. Same as DELETE /v1/jobs/{job}. On failure with notFound, the id is unknown or not owned.",
        capabilities: &[types::Capability::JobCancel], operation: Some(types::InterfaceOperation::CancelJob), schema: job_cancel_schema, args: JobIdArgs,
        phases: ACT | VERIFY, group: Workflow, hints: (false, true, false, false) }
    job_status { order: 28, title: "Job status", description: "Read one owned job by id. Requires job:read. Same as GET /v1/jobs/{job}. On failure with notFound, the id is unknown or not owned.",
        capabilities: &[types::Capability::JobRead], operation: Some(types::InterfaceOperation::ReadJob), schema: job_cancel_schema, args: JobIdArgs,
        phases: ACT | VERIFY, group: Workflow, hints: (true, false, false, false) }
    job_submit { order: 29, title: "Submit job", description: "Submit a built-in job: echo; sleep (ms); http_probe (url); http_wait (url, contains); http_fetch (url). Requires job:submit; HTTP handlers also require network:egress. Same as POST /v1/jobs. On failure after submission, use job_status on the returned id; unknown names fail asynchronously, so do not resubmit.",
        capabilities: &[types::Capability::JobSubmit], operation: Some(types::InterfaceOperation::SubmitJob), schema: job_submit_schema, args: JobSubmitArgs,
        phases: ACT | VERIFY, group: Workflow, hints: (false, false, false, false) }
    navigate { order: 30, title: "Navigate", description: "Navigate and wait for a load state. Requires browser:mutate. On failure, use a longer timeoutMs.",
        capabilities: &[types::Capability::BrowserMutate], operation: Some(types::InterfaceOperation::SubmitCommand), schema: navigate_schema, args: NavigateArgs,
        phases: EXPLORE | ACT | INTENT, scope: SessionPageWorkflow, group: Primitives, hints: (false, false, false, true) }
    network_log { order: 31, title: "Read network log", description: "Dump the page's recorded network log as a HAR artifact, then clear the buffer unless clear is false. Call it once before the traffic you want; recording starts at the first call. Requires browser:mutate. Produces HAR-artifact evidence. On failure retry; not caller-fixable.",
        capabilities: &[types::Capability::BrowserMutate], operation: Some(types::InterfaceOperation::SubmitCommand), schema: network_log_schema, args: NetworkLogArgs,
        phases: ACT, scope: SessionPageWorkflow, group: PageOps, hints: (false, false, false, false) }
    page_activate { order: 35, title: "Activate page", description: "Bring a page to the front. Requires browser:mutate. On failure with notFound, call page_list for current ids.",
        capabilities: &[types::Capability::BrowserMutate], operation: Some(types::InterfaceOperation::SubmitCommand), schema: page_activate_schema, args: PageCloseArgs,
        phases: ALWAYS, scope: SessionPageWorkflow, group: PageOps, hints: (false, false, false, false) }
    page_close { order: 36, title: "Close page", description: "Close a page in an owned session. Requires browser:mutate. Destructive. On failure with notFound, call page_list for current ids.",
        capabilities: &[types::Capability::BrowserMutate], operation: Some(types::InterfaceOperation::SubmitCommand), schema: page_close_schema, args: PageCloseArgs,
        phases: ALWAYS, scope: SessionPageWorkflow, group: PageOps, hints: (false, true, false, false) }
    page_list { order: 37, title: "List pages", description: "List open pages in an owned session. Requires browser:mutate.",
        capabilities: &[types::Capability::BrowserMutate], operation: Some(types::InterfaceOperation::SubmitCommand), schema: page_list_schema, args: PageListArgs,
        phases: ALWAYS, group: PageOps, hints: (true, false, false, false) }
    page_open { order: 38, title: "Open page", description: "Open a page, optionally navigating it. Requires page:write and browser:mutate when a URL is set. On failure with notFound, check session_list.",
        capabilities: &[types::Capability::PageWrite], operation: Some(types::InterfaceOperation::OpenPage), schema: page_open_schema, args: PageOpenArgs,
        phases: ALWAYS, group: Lifecycle, hints: (false, false, false, true) }
    pdf { order: 39, title: "Print to PDF", description: "Print a page to a PDF artifact with optional layout and scale. Requires browser:mutate. Produces a PDF artifact with its size and checksum. On failure with invalidRequest, scale is out of range -- pass a value between 0.1 and 2.0.",
        capabilities: &[types::Capability::BrowserMutate], operation: Some(types::InterfaceOperation::SubmitCommand), schema: pdf_schema, args: PdfArgs,
        phases: VERIFY, scope: SessionPageWorkflow, group: PageOps, hints: (false, false, false, false) }
    recovery_status { order: 40, title: "Recovery status", description: "Read a workflow's checkpoint and recovery receipts without attempting recovery, or pass sessionId instead of workflowId to list that session's recoverable workflows newest-first. Pass exactly one. Requires recovery:read.",
        capabilities: &[types::Capability::RecoveryRead], operation: Some(types::InterfaceOperation::ReadCheckpoint), schema: recovery_status_schema, args: RecoveryStatusArgs,
        phases: VERIFY, group: Workflow, hints: (true, false, false, false) }
    runtime_info { order: 41, title: "Runtime info", description: "Runtime version, active sessions, and credential expiry. Requires session:read.",
        capabilities: &[types::Capability::SessionRead], operation: Some(types::InterfaceOperation::RuntimeInfo), schema: runtime_info_schema, args: EmptyArgs,
        phases: ALWAYS, group: Lifecycle, hints: (true, false, false, false) }
    screenshot { order: 42, title: "Screenshot", description: "Capture a screenshot artifact of a page's viewport, full page, or one element. Requires browser:mutate.",
        capabilities: &[types::Capability::BrowserMutate], operation: Some(types::InterfaceOperation::SubmitCommand), schema: screenshot_schema, args: ScreenshotArgs,
        phases: VERIFY, scope: SessionPageWorkflow, group: Primitives, hints: (true, false, false, false) }
    session_close { order: 43, title: "Close session", description: "Close a session and release its resources. Requires session:write. Destructive. On failure, confirm with session_list.",
        capabilities: &[types::Capability::SessionWrite], operation: Some(types::InterfaceOperation::DeleteSession), schema: session_close_schema, args: SessionCloseArgs,
        phases: ALWAYS, group: Lifecycle, hints: (false, true, false, false) }
    session_create { order: 44, title: "Create session", description: "Create a browser session with a profile and execution policy. Requires session:write. On failure with resourceExhausted, close an idle session.",
        capabilities: &[types::Capability::SessionWrite], operation: Some(types::InterfaceOperation::CreateSession), schema: session_create_schema, args: SessionCreateArgs,
        phases: ALWAYS, group: Lifecycle, hints: (false, false, false, false) }
    session_list { order: 45, title: "List sessions", description: "List sessions visible to this principal. Requires session:read.",
        capabilities: &[types::Capability::SessionRead], operation: Some(types::InterfaceOperation::ReadSession), schema: runtime_info_schema, args: EmptyArgs,
        phases: ALWAYS, group: Lifecycle, hints: (true, false, false, false) }
    toolset_select { order: 46, title: "Select a toolset phase", description: "Narrow tools/list to one phase (explore, act, intent, verify, full). Requires no capability.",
        capabilities: &[], operation: None, schema: toolset_select_schema, args: ToolsetSelectArgs,
        phases: ALWAYS, group: Toolset, hints: (true, false, false, false) }
    type_text { order: 47, title: "Type text", description: "Type into an element by selector or resolved target, optionally clearing it first. Requires browser:mutate. On failure with targetNotFound or targetAmbiguous, refresh a11y_snapshot.",
        capabilities: &[types::Capability::BrowserMutate], operation: Some(types::InterfaceOperation::SubmitCommand), schema: type_text_schema, args: TypeTextArgs,
        phases: EXPLORE | ACT, scope: SessionPageWorkflow, group: Primitives, hints: (false, false, false, false) }
    upload_files { order: 48, title: "Upload files", description: "Set files on a file input via selector, target, or form_snapshot controlId. Requires browser:mutate and file:upload. On failure with needsReconciliation, call recovery_status; on policyDenied, use a configured upload root.",
        capabilities: &[
            types::Capability::BrowserMutate,
            types::Capability::FileUpload,
        ], operation: Some(types::InterfaceOperation::SubmitCommand), schema: upload_files_schema, args: UploadFilesArgs,
        phases: EXPLORE | ACT, scope: SessionPageWorkflow, group: PageOps, hints: (false, false, false, false) }
    wait_for { order: 49, title: "Wait for condition", description: "Wait for a page condition with a bounded timeout. Requires browser:mutate.",
        capabilities: &[types::Capability::BrowserMutate], operation: Some(types::InterfaceOperation::SubmitCommand), schema: wait_for_schema, args: WaitForArgs,
        phases: ACT | INTENT, scope: SessionPageWorkflow, group: Primitives, hints: (true, false, false, false) }
    workflow_observe { order: 51, title: "Observe retained workflow", description: "Observe retained or live accessibility evidence. Requires browser:mutate; forms need page:read. Defaults evidenceDetail=compact.",
        capabilities: WORKFLOW_OBSERVE_REQUIRED_CAPABILITIES, operation: Some(WORKFLOW_OBSERVE_OPERATION), schema: workflow_observe_schema, args: WorkflowObserveArgs,
        phases: ALWAYS, scope: SessionPageWorkflow, group: AgentWorkflow, page_derived: true, hints: (true, false, false, false) }
    workflow_recover { order: 52, title: "Recover workflow", description: "Recover from the last verified checkpoint. Requires recovery:write. Returns resume, restart, or reconciliation evidence. On failure with notFound, verify session ownership with session_list.",
        capabilities: &[types::Capability::RecoveryWrite], operation: Some(types::InterfaceOperation::RecoverWorkflow), schema: workflow_recover_schema, args: WorkflowRecoverArgs,
        phases: VERIFY, group: Workflow, hints: (false, false, false, false) }
    workflow_start { order: 50, title: "Start retained workflow", description: "Create and bind a session, page, and workflow, optionally navigating to url. Requires session:read, session:write, page:write. On failure, inspect session_list.",
        capabilities: WORKFLOW_START_REQUIRED_CAPABILITIES, operation: Some(WORKFLOW_START_OPERATION), schema: workflow_start_schema, args: WorkflowStartArgs,
        phases: ALWAYS, group: AgentWorkflow, hints: (false, false, false, true) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_descriptor_has_a_schema_and_narrow_phase() {
        assert!(DESCRIPTORS
            .windows(2)
            .all(|pair| pair[0].name < pair[1].name));
        for tool in DESCRIPTORS {
            assert!(!tool.title.is_empty(), "{} needs a title", tool.name);
            assert!(
                !tool.description.is_empty(),
                "{} needs a description",
                tool.name
            );
            assert!(tool.phases != 0, "{} has no narrow phase", tool.name);
            let schema = crate::schema::tool_schema(tool.name);
            assert!(
                schema["properties"].is_object(),
                "{} has no input schema",
                tool.name
            );
            if tool.scope.is_some() {
                assert!(
                    schema["properties"]["sessionId"].is_object(),
                    "{} has a scope without session arguments",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn form_intents_describe_the_compact_verified_loop() {
        let complete = tool_description("intent_complete_form");
        assert!(
            complete.contains("evidenceDetail=compact"),
            "whole-form intent must advertise compact success evidence"
        );
        assert!(
            complete.contains("just-in-time") && complete.contains("revealedBy"),
            "whole-form intent must explain that fields resolve just-in-time and name revealedBy as the activation hook for a field that only appears once revealed"
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
