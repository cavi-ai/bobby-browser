use serde_json::{json, Value};

pub const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
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
