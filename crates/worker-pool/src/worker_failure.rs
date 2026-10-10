use crate::targeting::TARGET_GONE_MESSAGE;
use types::CommandError;

/// The Firefox worker's message once its transport is gone or it was closed.
pub const FIREFOX_WORKER_CLOSED_MESSAGE: &str = "Firefox companion worker is closed";

/// Recognize a terminated Firefox BiDi transport.
pub fn is_firefox_bidi_transport_dead(message: &str) -> bool {
    message.contains("Firefox BiDi")
        && !message.contains("client closed")
        && (message.contains("connection closed")
            || message.contains("connection ended")
            || message.contains("disconnected")
            || message.contains("command channel closed")
            || message.contains("command capacity closed")
            || message.contains("response channel closed"))
}

/// What a caller is told when its session's browser is gone.
pub const BROWSER_GONE_MESSAGE: &str =
    "this session's browser is gone; create a new session and close this one";

/// The session's browser can never serve another command: a dead worker
/// (either engine) or a closed Firefox worker.
pub fn is_browser_gone_error(error: &types::CommandError) -> bool {
    is_dead_worker_error(error) || error.message == FIREFOX_WORKER_CLOSED_MESSAGE
}

pub(crate) fn is_closed_page_message(message: &str) -> bool {
    message.contains("receiver is gone")
        || message.contains("session closed")
        || message.contains("Session with given id not found")
        || message.contains("oneshot canceled")
        || message.contains(TARGET_GONE_MESSAGE)
        || is_firefox_bidi_transport_dead(message)
}

/// The worker's browser is gone or unreachable: dead command channel,
/// canceled oneshot, closed session, or an explicitly closed worker. Such a
/// worker can never serve another command, so callers may invalidate and
/// re-lease for a fresh browser instead of surfacing a dead-end failure.
pub fn is_dead_worker_error(error: &CommandError) -> bool {
    is_closed_page_message(&error.message) || error.message == "browser worker is closed"
}

#[cfg(test)]
mod tests {
    use super::*;
    use types::{ErrorCode, ErrorLayer};

    #[test]
    fn closed_page_messages_preserve_cross_engine_classification() {
        assert!(is_closed_page_message(
            "send failed because receiver is gone"
        ));
        assert!(is_closed_page_message("oneshot canceled"));
        assert!(!is_closed_page_message(
            "connection temporarily unavailable"
        ));
        assert!(is_closed_page_message("Firefox BiDi connection closed"));
        assert!(is_closed_page_message(
            "Firefox BiDi reader disconnected: connection reset"
        ));
        assert!(!is_closed_page_message("Firefox BiDi client closed"));
    }

    #[test]
    fn dead_target_error_rewritten_by_targeting_triggers_the_revive_path() {
        let rewritten = CommandError {
            code: ErrorCode::TargetDetached,
            message: TARGET_GONE_MESSAGE.into(),
            layer: ErrorLayer::Driver,
            retryable: true,
        };
        assert!(is_dead_worker_error(&rewritten));

        let stale_element = CommandError {
            code: ErrorCode::TargetDetached,
            message: "target has no live object".into(),
            layer: ErrorLayer::Driver,
            retryable: false,
        };
        assert!(!is_dead_worker_error(&stale_element));
    }
}
