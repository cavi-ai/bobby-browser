use serde::Deserialize;
use types::{CommandOutcome, VerificationStatus};

#[derive(Deserialize)]
struct Case {
    name: String,
    outcome: CommandOutcome,
    expected: VerificationStatus,
}

#[test]
fn verification_status_matches_the_shared_sdk_contract() {
    let cases: Vec<Case> = serde_json::from_str(include_str!(
        "../../../tests/fixtures/command-verification.json"
    ))
    .unwrap();
    for case in cases {
        assert_eq!(
            case.outcome.verification_status(),
            case.expected,
            "{}",
            case.name
        );
        let round: CommandOutcome =
            serde_json::from_value(serde_json::to_value(&case.outcome).unwrap()).unwrap();
        assert_eq!(
            round.verification_status(),
            case.expected,
            "{} round trip",
            case.name
        );
        assert_eq!(
            case.outcome.journal_safe().verification_status(),
            case.expected,
            "{} journal",
            case.name
        );
    }
}
