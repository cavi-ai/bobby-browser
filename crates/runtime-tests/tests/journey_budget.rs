//! Deterministic regression limits fail closed on missing or exceeded evidence.
#[path = "support/journey_budget.rs"]
mod journey_budget;

use journey_budget::{Budget, Usage};

const LIMIT: Budget = Budget {
    calls: 3,
    response_bytes: 100,
};

#[test]
fn limits_accept_exact_boundary_and_reject_each_overrun() {
    assert!(Usage {
        calls: 3,
        response_bytes: 100
    }
    .check(Some(LIMIT))
    .is_ok());
    assert!(Usage {
        calls: 4,
        response_bytes: 100
    }
    .check(Some(LIMIT))
    .is_err());
    assert!(Usage {
        calls: 3,
        response_bytes: 101
    }
    .check(Some(LIMIT))
    .is_err());
}

#[test]
fn missing_measurements_and_unknown_journeys_fail() {
    for usage in [
        Usage::default(),
        Usage {
            calls: 1,
            response_bytes: 0,
        },
        Usage {
            calls: 0,
            response_bytes: 1,
        },
    ] {
        assert!(usage.check(Some(LIMIT)).is_err());
    }
    assert!(Usage {
        calls: 1,
        response_bytes: 1
    }
    .check(None)
    .is_err());
    assert!(journey_budget::limit("unknown-engine", "j1_sign_in_redirect_then_app").is_none());
    assert!(journey_budget::limit("chromium", "unknown-journey").is_none());
}

#[test]
fn deltas_exclude_earlier_journeys_and_reject_counter_regression() {
    let earlier = Usage {
        calls: 7,
        response_bytes: 1000,
    };
    let later = Usage {
        calls: 10,
        response_bytes: 1100,
    };
    assert_eq!(
        later.since(earlier).unwrap(),
        Usage {
            calls: 3,
            response_bytes: 100
        }
    );
    assert!(earlier.since(later).is_err());
}

#[test]
fn meter_counts_full_serialized_json_responses_in_utf8_bytes() {
    let meter = journey_budget::Meter::default();
    let first = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"café"}],"structuredContent":{"status":"completed"}}});
    let second = serde_json::json!({"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"invalid arguments"}});
    meter.record(&first);
    let before = meter.snapshot();
    meter.record(&second);
    assert_eq!(before.calls, 1);
    assert_eq!(
        before.response_bytes,
        serde_json::to_vec(&first).unwrap().len() as u64
    );
    assert_eq!(
        meter.snapshot().since(before).unwrap(),
        Usage {
            calls: 1,
            response_bytes: serde_json::to_vec(&second).unwrap().len() as u64
        }
    );
}
