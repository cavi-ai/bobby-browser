use serde_json::{json, Value};

pub const MCP_PROTOCOL_VERSION: &str = "2025-11-25";

/// Every revision this gateway speaks, newest first.
///
/// Rejecting an older revision outright made the gateway unreachable from any host that
/// had not moved to the newest one: Claude Code offers 2025-06-18, got `Invalid params`,
/// and dropped the connection, so `bobby-browser` never appeared in its tool list at all
/// while `bobby doctor` reported the gateway healthy — the handshake it runs asks for the
/// newest revision, so it never saw what a real client sees.
///
/// The MCP lifecycle expects negotiation here: the server answers with a revision it
/// supports, and the client decides whether it can live with it. Only a revision this
/// gateway does not implement is an error.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// The revision to answer `initialize` with: the client's own when this gateway speaks it,
/// otherwise the newest, which is what the spec says to offer when there is no overlap.
pub fn negotiate_protocol_version(requested: &str) -> &'static str {
    SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .find(|v| **v == requested)
        .copied()
        .unwrap_or(MCP_PROTOCOL_VERSION)
}
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_INPUT_BYTES: usize = 256 * 1024;
pub const MAX_REQUEST_ID_BYTES: usize = 256;
pub const MAX_EVENT_LIMIT: usize = 256;

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
pub const NOT_INITIALIZED: i64 = -32002;
pub const INTERFACE_ERROR: i64 = -32000;
pub const REQUEST_CANCELLED: i64 = -32800;

pub fn success(id: Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":result})
}

pub fn error(id: Value, code: i64, message: &'static str, data: Option<Value>) -> Value {
    let mut error = json!({"code":code,"message":message});
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({"jsonrpc":"2.0","id":id,"error":error})
}
