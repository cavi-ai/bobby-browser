use serde_json::{json, Value};

/// MCP tool annotations.
///
/// These are host hints, nothing more. `required_capabilities` remains the
/// only authority over what a principal may call; a host that ignores every
/// annotation still cannot reach a tool the principal lacks.
pub(crate) fn tool_annotations(name: &str) -> Value {
    let read_only = matches!(
        name,
        "runtime_info"
            | "session_list"
            | "page_list"
            | "inspect"
            | "a11y_snapshot"
            | "form_snapshot"
            | "screenshot"
            | "events_read"
            | "recovery_status"
            | "cookie_get"
    );
    let destructive = matches!(name, "session_close" | "page_close" | "cookie_delete");
    // `idempotentHint` per MCP is unconditional: "calling the tool repeatedly
    // with the same arguments will have no additional effect." An optional
    // `idempotencyKey` does not establish that — without a caller-supplied
    // key, `session_create` mints a second session, `command_execute` and
    // every `intent_*` tool (e.g. `intent_fill`, which appends rather than
    // replaces) have no dedupe at all. Only two tools converge regardless of
    // any key: `checkpoint_save` (the store overwrites by `workflow_id`) and
    // `emulate` (sets viewport/geolocation to fixed values).
    let idempotent = matches!(name, "checkpoint_save" | "emulate");
    // `command_execute` accepts an arbitrary `RuntimeCommand`, which can be
    // `Navigate` or `DownloadUrl` — the same commands that earn the
    // standalone tools their `openWorldHint`. A host gating "does this reach
    // the network" on this flag would otherwise miss envelope-mediated
    // navigation entirely.
    let open_world = matches!(
        name,
        "navigate" | "download_url" | "extract_structured" | "command_execute"
    );
    json!({
        "readOnlyHint": read_only,
        "destructiveHint": destructive,
        "idempotentHint": idempotent,
        "openWorldHint": open_world
    })
}

pub(crate) fn tool_title(name: &str) -> &'static str {
    // Every tool in `list_tools`'s name list (`server.rs`) has an explicit arm
    // below, so the wildcard is unreachable in practice — proven by
    // `every_tool_carries_a_title_and_annotations` in `tests/budget.rs`. It
    // still has to return `&'static str`, so it can't echo the (non-static)
    // input back; a fixed fallback keeps the function total.
    match name {
        "a11y_snapshot" => "Accessibility snapshot",
        "checkpoint_save" => "Save checkpoint",
        "click" => "Click",
        "command_execute" => "Execute command envelope",
        "control_action" => "Form control action",
        "cookie_delete" => "Delete cookies",
        "cookie_get" => "Read cookies",
        "cookie_set" => "Set cookies",
        "dialog" => "Handle dialog",
        "download_url" => "Download URL",
        "emulate" => "Emulate device",
        "evaluate_javascript" => "Evaluate JavaScript",
        "events_read" => "Read events",
        "extract_structured" => "Extract structured data",
        "form_snapshot" => "Form snapshot",
        "inspect" => "Inspect page",
        "intent_complete_form" => "Complete form",
        "intent_dismiss_obstruction" => "Dismiss obstruction",
        "intent_extract" => "Extract fields",
        "intent_fill" => "Fill control",
        "intent_follow" => "Follow link",
        "intent_locate" => "Locate element",
        "intent_submit_and_verify" => "Submit and verify",
        "intent_wait_for_state" => "Wait for state",
        "navigate" => "Navigate",
        "network_log" => "Read network log",
        "page_activate" => "Activate page",
        "page_close" => "Close page",
        "page_list" => "List pages",
        "page_open" => "Open page",
        "pdf" => "Print to PDF",
        "recovery_status" => "Recovery status",
        "runtime_info" => "Runtime info",
        "screenshot" => "Screenshot",
        "session_close" => "Close session",
        "session_create" => "Create session",
        "session_list" => "List sessions",
        "type_text" => "Type text",
        "upload_files" => "Upload files",
        "wait_for" => "Wait for condition",
        "workflow_recover" => "Recover workflow",
        _ => "Untitled tool",
    }
}
