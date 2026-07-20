use release_gates::{evaluate, CertificationVerdict, GateResult, GateStatus, PolicyError};

fn result(suite: &str, check: &str, required: bool, status: GateStatus) -> GateResult {
    GateResult::new(suite, check, required, status, 1, vec![])
}

#[test]
fn policy_is_fail_closed_and_degradation_is_distinct() {
    assert_eq!(
        evaluate(
            &["security"],
            &[result("security", "all", true, GateStatus::Passed)]
        )
        .unwrap(),
        CertificationVerdict::Passed
    );
    assert_eq!(
        evaluate(
            &["security"],
            &[result("security", "all", true, GateStatus::Degraded)]
        )
        .unwrap(),
        CertificationVerdict::Degraded
    );
    assert_eq!(
        evaluate(
            &["security"],
            &[result("security", "all", true, GateStatus::Blocked)]
        )
        .unwrap(),
        CertificationVerdict::Blocked
    );
    assert_eq!(
        evaluate(
            &["security"],
            &[
                result("security", "availability", true, GateStatus::Degraded),
                result("security", "confidentiality", true, GateStatus::Blocked),
            ]
        )
        .unwrap(),
        CertificationVerdict::Blocked
    );
    assert!(matches!(
        evaluate(&["security"], &[]),
        Err(PolicyError::MissingRequiredSuite(name)) if name == "security"
    ));
    assert!(matches!(
        evaluate(
            &["security"],
            &[
                result("security", "one", true, GateStatus::Passed),
                result("security", "one", true, GateStatus::Passed),
            ]
        ),
        Err(PolicyError::DuplicateCheck { suite, check })
            if suite == "security" && check == "one"
    ));
}

#[test]
fn policy_allows_distinct_checks_in_the_same_suite() {
    assert_eq!(
        evaluate(
            &["security"],
            &[
                result("security", "availability", true, GateStatus::Passed),
                result("security", "confidentiality", true, GateStatus::Passed),
            ]
        )
        .unwrap(),
        CertificationVerdict::Passed
    );
}

#[test]
fn policy_rejects_empty_identifiers() {
    assert!(matches!(evaluate(&[""], &[]), Err(PolicyError::EmptySuite)));
    assert!(matches!(
        evaluate(
            &["security"],
            &[result("", "check", true, GateStatus::Passed)]
        ),
        Err(PolicyError::EmptySuite)
    ));
    assert!(matches!(
        evaluate(
            &["security"],
            &[result("security", "", true, GateStatus::Passed)]
        ),
        Err(PolicyError::EmptyCheck)
    ));
}

#[test]
fn policy_requires_every_named_suite_regardless_of_result_required_flag() {
    assert!(matches!(
        evaluate(
            &["security", "reliability"],
            &[result("security", "all", false, GateStatus::Passed)]
        ),
        Err(PolicyError::MissingRequiredSuite(name)) if name == "reliability"
    ));
}
