use companion_protocol::{BrowserEngine, BrowserIdentity, InteractionPath};
use release_gates::{
    evaluate_native_browser_proof, NativeBrowserOperationProof, NativeBrowserProof,
    NativeBrowserProofError,
};

fn complete_proof() -> NativeBrowserProof {
    NativeBrowserProof {
        browser: Some(BrowserIdentity {
            engine: BrowserEngine::Firefox,
            browser_name: "Firefox".into(),
            browser_version: "stable".into(),
            os: "macos".into(),
            profile_label: "native-proof".into(),
        }),
        operations: [
            ("navigate", InteractionPath::EngineNative),
            ("inspect", InteractionPath::ExtensionApi),
            ("click", InteractionPath::EngineNative),
            ("typeText", InteractionPath::EngineNative),
        ]
        .into_iter()
        .map(|(name, interaction_path)| NativeBrowserOperationProof {
            name: name.into(),
            interaction_path,
            postcondition_verified: true,
            duration_ms: 10,
        })
        .collect(),
        confirmation_text: "Submitted".into(),
        evidence: Vec::new(),
        redaction_findings: Vec::new(),
        elapsed_ms: 40,
        deadline_ms: 1_000,
    }
}

#[test]
fn complete_firefox_native_input_proof_passes_with_exact_identity() {
    let proof = complete_proof();
    evaluate_native_browser_proof(&proof).unwrap();
    let browser = proof.browser.unwrap();
    assert_eq!(browser.engine, BrowserEngine::Firefox);
    assert_eq!(browser.profile_label, "native-proof");
    assert_eq!(
        proof.operations[2].interaction_path,
        InteractionPath::EngineNative
    );
    assert_eq!(
        proof.operations[3].interaction_path,
        InteractionPath::EngineNative
    );
}

#[test]
fn missing_browser_identity_is_incomplete() {
    let mut proof = complete_proof();
    proof.browser = None;
    assert_eq!(
        evaluate_native_browser_proof(&proof),
        Err(NativeBrowserProofError::MissingBrowserIdentity)
    );
}

#[test]
fn exact_complete_browser_identity_is_required() {
    for field in ["browserName", "browserVersion", "os", "profileLabel"] {
        let mut proof = complete_proof();
        let browser = proof.browser.as_mut().unwrap();
        match field {
            "browserName" => browser.browser_name.clear(),
            "browserVersion" => browser.browser_version.clear(),
            "os" => browser.os.clear(),
            "profileLabel" => browser.profile_label.clear(),
            _ => unreachable!(),
        }
        assert_eq!(
            evaluate_native_browser_proof(&proof),
            Err(NativeBrowserProofError::IncompleteBrowserIdentity(
                field.into()
            ))
        );
    }
    let mut proof = complete_proof();
    proof.browser.as_mut().unwrap().browser_name = "Firefox-compatible".into();
    assert_eq!(
        evaluate_native_browser_proof(&proof),
        Err(NativeBrowserProofError::WrongBrowserName)
    );
}

#[test]
fn extension_fallback_cannot_certify_required_native_input() {
    let mut proof = complete_proof();
    proof.operations[3].interaction_path = InteractionPath::ExtensionApi;
    assert_eq!(
        evaluate_native_browser_proof(&proof),
        Err(NativeBrowserProofError::NonNativeInput("typeText".into()))
    );
}

#[test]
fn redaction_finding_fails_closed_without_repeating_the_secret() {
    let mut proof = complete_proof();
    proof.redaction_findings.push("authorization header".into());
    assert_eq!(
        evaluate_native_browser_proof(&proof),
        Err(NativeBrowserProofError::RedactionFindings(1))
    );
}

#[test]
fn secret_marker_in_retained_evidence_fails_closed() {
    let mut proof = complete_proof();
    proof
        .evidence
        .push("Authorization: Bearer must-not-be-retained".into());
    assert_eq!(
        evaluate_native_browser_proof(&proof),
        Err(NativeBrowserProofError::SensitiveEvidence)
    );
}

#[test]
fn exact_confirmation_and_bounded_operation_timings_are_required() {
    let mut proof = complete_proof();
    proof.confirmation_text = "Submitted maybe".into();
    assert_eq!(
        evaluate_native_browser_proof(&proof),
        Err(NativeBrowserProofError::UnexpectedConfirmation)
    );
    let mut proof = complete_proof();
    proof.operations[0].duration_ms = 0;
    assert_eq!(
        evaluate_native_browser_proof(&proof),
        Err(NativeBrowserProofError::TimingOutOfBounds)
    );
}

#[test]
fn every_required_verified_postcondition_is_mandatory() {
    for name in ["navigate", "inspect", "click", "typeText"] {
        let mut proof = complete_proof();
        proof.operations.retain(|operation| operation.name != name);
        assert_eq!(
            evaluate_native_browser_proof(&proof),
            Err(NativeBrowserProofError::MissingVerifiedOperation(
                name.into()
            ))
        );
    }
}

#[test]
fn proof_requires_exactly_one_record_for_each_operation() {
    let mut proof = complete_proof();
    proof.operations.push(proof.operations[0].clone());
    assert_eq!(
        evaluate_native_browser_proof(&proof),
        Err(NativeBrowserProofError::InvalidOperationSet)
    );
}

#[test]
fn proof_must_finish_within_its_positive_deadline() {
    let mut proof = complete_proof();
    proof.elapsed_ms = proof.deadline_ms + 1;
    assert_eq!(
        evaluate_native_browser_proof(&proof),
        Err(NativeBrowserProofError::TimingOutOfBounds)
    );
    proof.elapsed_ms = 0;
    assert_eq!(
        evaluate_native_browser_proof(&proof),
        Err(NativeBrowserProofError::TimingOutOfBounds)
    );
}
