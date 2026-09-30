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

/// A JSON-RPC error whose `message` ends with the repair action. Hosts
/// (Claude Code among them) render only `error.message`, so a repair parked
/// in `data` alone reaches no one. The site's own `data.repair` wins; a site
/// with none gets the code's general repair on `data.repair` as well.
pub fn error(id: Value, code: i64, message: &'static str, data: Option<Value>) -> Value {
    let mut data = data.unwrap_or_else(|| json!({}));
    if let Some(fields) = data.as_object_mut() {
        if !fields
            .get("repair")
            .is_some_and(|repair| repair["action"].is_string())
        {
            fields.insert(
                "repair".to_owned(),
                crate::repair::repair_for_rpc_code(code),
            );
        }
    }
    let message = match data["repair"]["action"].as_str() {
        Some(action) => format!("{message}; repair: {action}"),
        None => message.to_owned(),
    };
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message,"data":data}})
}

#[cfg(test)]
mod tests {
    use super::*;

    const RPC_CODES: [(i64, &str); 8] = [
        (PARSE_ERROR, "Parse error"),
        (INVALID_REQUEST, "Invalid Request"),
        (METHOD_NOT_FOUND, "Method not found"),
        (INVALID_PARAMS, "Invalid params"),
        (INTERNAL_ERROR, "Internal error"),
        (NOT_INITIALIZED, "Server not initialized"),
        (INTERFACE_ERROR, "Runtime interface error"),
        (REQUEST_CANCELLED, "Request cancelled"),
    ];

    /// Every error this gateway can emit names its repair in `error.message`,
    /// the one field hosts render, and carries it on `error.data.repair`.
    #[test]
    fn every_rpc_error_message_ends_with_its_repair_action() {
        for (code, base) in RPC_CODES {
            let response = error(json!(1), code, base, None);
            let repair = crate::repair::repair_for_rpc_code(code);
            let action = repair["action"].as_str().expect("repair action");
            assert_eq!(
                response["error"]["message"],
                json!(format!("{base}; repair: {action}")),
                "{response}"
            );
            assert_eq!(response["error"]["data"]["repair"], repair, "{response}");
        }
    }

    #[test]
    fn a_site_repair_wins_and_its_other_data_is_kept() {
        let site = json!({"action":"Do the specific thing.","doc":"bobby://failure-taxonomy"});
        let response = error(
            json!(2),
            INVALID_REQUEST,
            "Invalid Request",
            Some(json!({"reason":"frameTooLarge","repair":site.clone()})),
        );
        assert_eq!(
            response["error"]["message"],
            "Invalid Request; repair: Do the specific thing."
        );
        assert_eq!(response["error"]["data"]["repair"], site);
        assert_eq!(response["error"]["data"]["reason"], "frameTooLarge");
    }

    #[test]
    fn site_data_without_a_repair_gets_the_code_repair_beside_it() {
        let response = error(
            json!(3),
            INTERNAL_ERROR,
            "Internal error",
            Some(json!({"reason":"resultTooLarge"})),
        );
        assert_eq!(response["error"]["data"]["reason"], "resultTooLarge");
        assert_eq!(
            response["error"]["data"]["repair"],
            crate::repair::repair_for_rpc_code(INTERNAL_ERROR)
        );
    }
}
