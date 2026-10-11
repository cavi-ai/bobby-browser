use types::{CommandError, ErrorCode, ErrorLayer};
use worker_pool::{
    is_browser_gone_error, is_dead_worker_error, is_firefox_bidi_transport_dead,
    FIREFOX_WORKER_CLOSED_MESSAGE,
};

fn error(message: &str) -> CommandError {
    CommandError {
        code: ErrorCode::BrowserCommandFailed,
        message: message.into(),
        layer: ErrorLayer::Driver,
        retryable: false,
    }
}

#[test]
fn bidi_transport_failures_are_shared_without_classifying_client_closure_as_worker_death() {
    for message in [
        "Firefox BiDi connection closed",
        "Firefox BiDi connection ended",
        "Firefox BiDi reader disconnected: connection reset",
        "Firefox BiDi command channel closed",
        "Firefox BiDi command capacity closed",
        "Firefox BiDi response channel closed",
    ] {
        assert!(is_firefox_bidi_transport_dead(message), "{message}");
        assert!(is_dead_worker_error(&error(message)), "{message}");
        assert!(is_browser_gone_error(&error(message)), "{message}");
    }
    for message in [
        "Firefox BiDi client closed",
        "Firefox BiDi client closed: connection closed",
        "connection temporarily unavailable",
        "target has no live object",
    ] {
        assert!(!is_firefox_bidi_transport_dead(message), "{message}");
        assert!(!is_dead_worker_error(&error(message)), "{message}");
        assert!(!is_browser_gone_error(&error(message)), "{message}");
    }
}

#[test]
fn explicit_worker_closure_preserves_the_public_predicate_distinction() {
    assert!(is_dead_worker_error(&error("browser worker is closed")));
    assert!(is_browser_gone_error(&error("browser worker is closed")));
    let firefox = error(FIREFOX_WORKER_CLOSED_MESSAGE);
    assert!(!is_dead_worker_error(&firefox));
    assert!(is_browser_gone_error(&firefox));
}
