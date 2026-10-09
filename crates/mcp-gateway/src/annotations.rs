use serde_json::{json, Value};

/// MCP tool annotations.
///
/// These are host hints, nothing more. `required_capabilities` remains the
/// only authority over what a principal may call; a host that ignores every
/// annotation still cannot reach a tool the principal lacks.
pub(crate) fn tool_annotations(name: &str) -> Value {
    let tool = crate::catalog::descriptor(name);
    let read_only = tool.is_some_and(|tool| tool.read_only);
    let destructive = tool.is_some_and(|tool| tool.destructive);
    let idempotent = tool.is_some_and(|tool| tool.idempotent);
    let open_world = tool.is_some_and(|tool| tool.open_world);
    // Every catalog entry carries these, so hints equal to the MCP default of
    // `false` (`readOnlyHint`, `idempotentHint`) are left out, and so is
    // `destructiveHint` on a read-only tool, where MCP says it has no meaning.
    // `destructiveHint` and `openWorldHint` default to `true`, so a `false`
    // for either is always written out.
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

pub(crate) fn tool_title(name: &str) -> &'static str {
    // Every tool in `list_tools`'s name list (`server.rs`) has an explicit arm
    // below, so the wildcard is unreachable in practice — proven by
    // `every_tool_carries_a_title_and_annotations` in `tests/budget.rs`. It
    // still has to return `&'static str`, so it can't echo the (non-static)
    // input back; a fixed fallback keeps the function total.
    crate::catalog::descriptor(name).map_or("Untitled tool", |tool| tool.title)
}
