use std::collections::{BTreeMap, BTreeSet};

use chrono::Utc;
use serde_json::json;
use types::{
    SessionId, SkillBrowserEngine, SkillCapability, SkillCommand, SkillDecision, SkillEvidenceRef,
    SkillFailure, SkillGhostCommand, SkillOutcome, SkillProfile, SkillProfileRequest,
    SkillSessionState, SkillTactic, SkillZigZagZigCommand,
};

fn capabilities(count: usize) -> BTreeSet<SkillCapability> {
    [
        SkillCapability::EngineSelection,
        SkillCapability::ProfilePersistence,
        SkillCapability::Locale,
        SkillCapability::Timezone,
        SkillCapability::Viewport,
        SkillCapability::UserAgentConsistency,
        SkillCapability::InteractionCadence,
    ]
    .into_iter()
    .cycle()
    .take(count)
    .collect()
}

fn evidence() -> SkillEvidenceRef {
    SkillEvidenceRef::new("screenshot-1", "a".repeat(64)).unwrap()
}

#[test]
fn skill_commands_and_failures_have_stable_camel_case_wire_shapes() {
    assert_eq!(
        serde_json::to_value(SkillCommand::Ghost(SkillGhostCommand::Status)).unwrap(),
        json!({"skill": "ghost", "action": "status"})
    );
    assert_eq!(
        serde_json::to_value(SkillFailure::StrategyExhausted).unwrap(),
        json!("strategyExhausted")
    );
}

#[test]
fn effective_profile_digest_cannot_serialize_secret_material() {
    assert!(SkillProfile::new(
        "v1",
        SkillBrowserEngine::Firefox,
        [SkillCapability::Locale],
        "cookie=secret"
    )
    .is_err());
}

#[test]
fn profile_request_rejects_unknown_fields_bad_schema_and_oversized_collections() {
    let valid = json!({
        "schemaVersion": 1,
        "required": ["locale"],
        "optional": [],
        "preferredEngines": ["firefox"],
        "values": {"locale": "en-US"}
    });
    assert!(serde_json::from_value::<SkillProfileRequest>(valid.clone()).is_ok());

    let mut invalid_schema = valid.clone();
    invalid_schema["schemaVersion"] = json!(0);
    assert!(serde_json::from_value::<SkillProfileRequest>(invalid_schema).is_err());

    let mut unknown_field = valid;
    unknown_field["unexpected"] = json!(true);
    assert!(serde_json::from_value::<SkillProfileRequest>(unknown_field).is_err());

    let oversized_values =
        BTreeMap::from_iter((0..65).map(|i| (format!("name-{i}"), "value".into())));
    assert!(SkillProfileRequest::new(
        [SkillCapability::Locale],
        [],
        [SkillBrowserEngine::Firefox],
        oversized_values,
    )
    .is_err());
}

#[test]
fn skill_profile_and_decision_validate_bounded_wire_values() {
    assert!(SkillProfile::new(
        "v".repeat(129),
        SkillBrowserEngine::Firefox,
        capabilities(1),
        "safe digest"
    )
    .is_err());
    assert!(SkillDecision::new(
        SkillTactic::ObserveAgain,
        SkillFailure::TargetDrift,
        "x".repeat(1025),
        1,
        1,
        None,
        None,
    )
    .is_err());
}

#[test]
fn evidence_refs_require_lowercase_sha256() {
    assert!(SkillEvidenceRef::new("artifact", "A".repeat(64)).is_err());
    assert!(SkillEvidenceRef::new("artifact", "a".repeat(63)).is_err());
    assert!(SkillEvidenceRef::new("a".repeat(129), "a".repeat(64)).is_err());
}

#[test]
fn session_state_and_outcomes_round_trip_and_limit_evidence() {
    let state = SkillSessionState::new(
        SessionId::new(),
        BTreeMap::from([("SkillGhost".into(), "v1".into())]),
        Some(
            SkillProfile::new(
                "v1",
                SkillBrowserEngine::Firefox,
                [SkillCapability::Locale],
                "locale=en-US",
            )
            .unwrap(),
        ),
        None,
        None,
        None,
        None,
        vec![SkillTactic::ObserveAgain],
        vec![evidence()],
        Utc::now(),
    )
    .unwrap();
    let value = serde_json::to_value(&state).unwrap();
    assert_eq!(value["schemaVersion"], 1);
    assert!(value.get("active_versions").is_none());
    assert_eq!(
        serde_json::from_value::<SkillSessionState>(value).unwrap(),
        state
    );

    assert!(SkillOutcome::applied(vec![evidence(); 129]).is_err());
}

#[test]
fn every_skill_contract_variant_round_trips_through_json() {
    for command in [
        SkillCommand::Ghost(SkillGhostCommand::On),
        SkillCommand::Ghost(SkillGhostCommand::Off),
        SkillCommand::Ghost(SkillGhostCommand::Status),
        SkillCommand::ZigZagZig(SkillZigZagZigCommand::Run),
        SkillCommand::ZigZagZig(SkillZigZagZigCommand::Status),
        SkillCommand::ZigZagZig(SkillZigZagZigCommand::Stop),
    ] {
        assert_eq!(
            serde_json::from_value::<SkillCommand>(serde_json::to_value(&command).unwrap())
                .unwrap(),
            command
        );
    }

    for capability in [
        SkillCapability::EngineSelection,
        SkillCapability::ProfilePersistence,
        SkillCapability::Locale,
        SkillCapability::Timezone,
        SkillCapability::Viewport,
        SkillCapability::UserAgentConsistency,
        SkillCapability::InteractionCadence,
    ] {
        assert_eq!(
            serde_json::from_value::<SkillCapability>(serde_json::to_value(capability).unwrap())
                .unwrap(),
            capability
        );
    }

    for failure in [
        SkillFailure::UnsupportedCapability,
        SkillFailure::ConfigurationConflict,
        SkillFailure::DeadlineExceeded,
        SkillFailure::TargetDrift,
        SkillFailure::PostconditionFailed,
        SkillFailure::EffectUncertain,
        SkillFailure::CheckpointMismatch,
        SkillFailure::StrategyExhausted,
        SkillFailure::EngineUnavailable,
    ] {
        assert_eq!(
            serde_json::from_value::<SkillFailure>(serde_json::to_value(failure).unwrap()).unwrap(),
            failure
        );
    }

    for tactic in [
        SkillTactic::ObserveAgain,
        SkillTactic::ResolveSemanticTarget,
        SkillTactic::ChangeInteractionMethod,
        SkillTactic::SolveChallenge,
        SkillTactic::ReconcileCheckpoint,
        SkillTactic::FreshGhostSession,
        SkillTactic::SelectCompatibleEngine,
        SkillTactic::RestartDurableBoundary,
    ] {
        assert_eq!(
            serde_json::from_value::<SkillTactic>(serde_json::to_value(tactic).unwrap()).unwrap(),
            tactic
        );
    }
    // The wire name is the contract: the ladder tactic shares it with the
    // intent kind so journals read consistently across both surfaces.
    assert_eq!(
        serde_json::to_value(SkillTactic::SolveChallenge).unwrap(),
        serde_json::json!("solveChallenge")
    );

    for engine in [
        SkillBrowserEngine::Firefox,
        SkillBrowserEngine::Chromium,
        SkillBrowserEngine::WebKit,
    ] {
        assert_eq!(
            serde_json::from_value::<SkillBrowserEngine>(serde_json::to_value(engine).unwrap())
                .unwrap(),
            engine
        );
    }

    let profile = SkillProfile::new(
        "v1",
        SkillBrowserEngine::Firefox,
        [SkillCapability::Locale],
        "locale=en-US",
    )
    .unwrap();
    let request = SkillProfileRequest::new(
        [SkillCapability::Locale],
        [SkillCapability::Viewport],
        [SkillBrowserEngine::Firefox],
        BTreeMap::from([("locale".into(), "en-US".into())]),
    )
    .unwrap();
    let decision = SkillDecision::new(
        SkillTactic::ObserveAgain,
        SkillFailure::TargetDrift,
        "target resolves to the expected semantic control",
        100,
        20,
        None,
        None,
    )
    .unwrap();
    let evidence_ref = evidence();
    let outcomes = [
        SkillOutcome::applied(vec![evidence_ref.clone()]).unwrap(),
        SkillOutcome::adapted(SkillTactic::ObserveAgain, vec![evidence_ref.clone()]).unwrap(),
        SkillOutcome::degraded(
            BTreeSet::from([SkillCapability::Viewport]),
            vec![evidence_ref.clone()],
        )
        .unwrap(),
        SkillOutcome::stopped(vec![evidence_ref.clone()]).unwrap(),
        SkillOutcome::failed(SkillFailure::DeadlineExceeded, vec![evidence_ref.clone()]).unwrap(),
    ];

    assert_eq!(
        serde_json::from_value::<SkillProfileRequest>(serde_json::to_value(&request).unwrap())
            .unwrap(),
        request
    );
    assert_eq!(
        serde_json::from_value::<SkillProfile>(serde_json::to_value(&profile).unwrap()).unwrap(),
        profile
    );
    assert_eq!(
        serde_json::from_value::<SkillDecision>(serde_json::to_value(&decision).unwrap()).unwrap(),
        decision
    );
    assert_eq!(
        serde_json::from_value::<SkillEvidenceRef>(serde_json::to_value(&evidence_ref).unwrap())
            .unwrap(),
        evidence_ref
    );
    for outcome in outcomes {
        assert_eq!(
            serde_json::from_value::<SkillOutcome>(serde_json::to_value(&outcome).unwrap())
                .unwrap(),
            outcome
        );
    }
}

#[test]
fn profile_request_rejects_capabilities_that_are_both_required_and_optional() {
    assert!(SkillProfileRequest::new(
        [SkillCapability::Locale],
        [SkillCapability::Locale],
        [SkillBrowserEngine::Firefox],
        BTreeMap::new(),
    )
    .is_err());

    assert!(serde_json::from_value::<SkillProfileRequest>(json!({
        "schemaVersion": 1,
        "required": ["locale"],
        "optional": ["locale"],
        "preferredEngines": ["firefox"],
        "values": {}
    }))
    .is_err());
}

#[test]
fn decision_rejects_tactic_budgets_that_exceed_the_remaining_deadline() {
    assert!(SkillDecision::new(
        SkillTactic::ObserveAgain,
        SkillFailure::TargetDrift,
        "target resolves to the expected semantic control",
        10,
        11,
        None,
        None,
    )
    .is_err());

    assert!(serde_json::from_value::<SkillDecision>(json!({
        "tactic": "observeAgain",
        "trigger": "targetDrift",
        "expectedPostcondition": "target resolves to the expected semantic control",
        "remainingDeadlineMs": 10,
        "tacticBudgetMs": 11,
        "checkpointId": null
    }))
    .is_err());
}

#[test]
fn session_state_rejects_unchecked_embedded_profile_and_evidence_json() {
    let session_id = SessionId::new();
    let valid = json!({
        "schemaVersion": 1,
        "sessionId": session_id,
        "activeVersions": {"SkillGhost": "v1"},
        "effectiveProfile": {
            "schemaVersion": 1,
            "version": "v1",
            "engine": "firefox",
            "effectiveCapabilities": ["locale"],
            "observableDigest": "locale=en-US"
        },
        "lastCheckpointId": null,
        "attemptedTactics": [],
        "evidence": [{"artifactId": "capture", "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}],
        "deadline": "2026-07-21T00:00:00Z"
    });
    assert!(serde_json::from_value::<SkillSessionState>(valid.clone()).is_ok());

    let mut secret_profile = valid.clone();
    secret_profile["effectiveProfile"]["observableDigest"] = json!("Authorization: bearer secret");
    assert!(serde_json::from_value::<SkillSessionState>(secret_profile).is_err());

    let mut invalid_evidence = valid;
    invalid_evidence["evidence"][0]["sha256"] = json!("not-a-sha256");
    assert!(serde_json::from_value::<SkillSessionState>(invalid_evidence).is_err());
}

#[test]
fn structured_contracts_reject_unknown_fields_and_zero_schema_versions() {
    assert!(serde_json::from_value::<SkillProfile>(json!({
        "schemaVersion": 0,
        "version": "v1",
        "engine": "firefox",
        "effectiveCapabilities": [],
        "observableDigest": "locale=en-US"
    }))
    .is_err());
    assert!(serde_json::from_value::<SkillDecision>(json!({
        "tactic": "observeAgain",
        "trigger": "targetDrift",
        "expectedPostcondition": "resolved",
        "remainingDeadlineMs": 1,
        "tacticBudgetMs": 1,
        "checkpointId": null,
        "unexpected": true
    }))
    .is_err());
    assert!(serde_json::from_value::<SkillEvidenceRef>(json!({
        "artifactId": "capture",
        "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "unexpected": true
    }))
    .is_err());
}

#[test]
fn constructors_reject_credential_like_and_absolute_path_metadata() {
    for artifact_id in ["token=secret", "/var/tmp/capture", r"C:\\capture"] {
        assert!(SkillEvidenceRef::new(artifact_id, "a".repeat(64)).is_err());
    }
    assert!(SkillProfile::new(
        "Bearer secret",
        SkillBrowserEngine::Firefox,
        [SkillCapability::Locale],
        "locale=en-US",
    )
    .is_err());
    assert!(SkillProfileRequest::new(
        [SkillCapability::Locale],
        [],
        [SkillBrowserEngine::Firefox],
        BTreeMap::from([("locale".into(), "authorization=secret".into())]),
    )
    .is_err());
    assert!(SkillProfileRequest::new(
        [SkillCapability::Locale],
        [],
        [SkillBrowserEngine::Firefox],
        BTreeMap::from([(r"C:\\profile".into(), "en-US".into())]),
    )
    .is_err());
}

#[test]
fn session_state_constructor_rejects_unsafe_active_version_metadata() {
    let profile = SkillProfile::new(
        "v1",
        SkillBrowserEngine::Firefox,
        [SkillCapability::Locale],
        "locale=en-US",
    )
    .unwrap();
    for active_versions in [
        BTreeMap::from([("token".into(), "v1".into())]),
        BTreeMap::from([("ghost".into(), "/absolute/version".into())]),
    ] {
        assert!(SkillSessionState::new(
            SessionId::new(),
            active_versions,
            Some(profile.clone()),
            None,
            None,
            None,
            None,
            Vec::new(),
            vec![evidence()],
            Utc::now(),
        )
        .is_err());
    }
}

#[test]
fn invalid_public_struct_literals_fail_closed_during_serialization() {
    let profile = SkillProfile {
        schema_version: 1,
        version: "v1".into(),
        engine: SkillBrowserEngine::Firefox,
        effective_capabilities: BTreeSet::from([SkillCapability::Locale]),
        observable_digest: "token=secret".into(),
    };
    assert!(serde_json::to_value(&profile).is_err());

    let request = SkillProfileRequest {
        schema_version: 1,
        required: BTreeSet::from([SkillCapability::Locale]),
        optional: BTreeSet::from([SkillCapability::Locale]),
        preferred_engines: vec![SkillBrowserEngine::Firefox],
        values: BTreeMap::new(),
    };
    assert!(serde_json::to_value(&request).is_err());

    let decision = SkillDecision {
        tactic: SkillTactic::ObserveAgain,
        trigger: SkillFailure::TargetDrift,
        expected_postcondition: "resolved".into(),
        remaining_deadline_ms: 10,
        tactic_budget_ms: 11,
        checkpoint_id: None,
        selected_engine: None,
    };
    assert!(serde_json::to_value(&decision).is_err());

    let evidence_ref = SkillEvidenceRef {
        artifact_id: "/var/tmp/capture".into(),
        sha256: "a".repeat(64),
    };
    assert!(serde_json::to_value(&evidence_ref).is_err());

    let state = SkillSessionState {
        schema_version: 1,
        session_id: SessionId::new(),
        active_versions: BTreeMap::from([("SkillGhost".into(), "v1".into())]),
        effective_profile: Some(profile),
        last_checkpoint_id: None,
        verified_checkpoint: None,
        reserved_tactic: None,
        pending_issuance: None,
        attempted_tactics: Vec::new(),
        evidence: vec![evidence()],
        deadline: Utc::now(),
    };
    assert!(serde_json::to_value(&state).is_err());
}

#[test]
fn field_specific_metadata_grammars_reject_named_bypasses() {
    for artifact_id in [
        "Basic YWxpY2U6c2VjcmV0",
        "ghp_secret",
        "api_key=secret",
        "../secrets",
        "~/secrets",
    ] {
        assert!(SkillEvidenceRef::new(artifact_id, "a".repeat(64)).is_err());
    }
    assert!(SkillProfileRequest::new(
        [SkillCapability::Locale],
        [],
        [SkillBrowserEngine::Firefox],
        BTreeMap::from([("locale".into(), "Basic YWxpY2U6c2VjcmV0".into())]),
    )
    .is_err());
    assert!(SkillProfileRequest::new(
        [SkillCapability::Timezone],
        [],
        [SkillBrowserEngine::Firefox],
        BTreeMap::from([("timezone".into(), "../secrets".into())]),
    )
    .is_err());
    assert!(SkillProfileRequest::new(
        [SkillCapability::Viewport],
        [],
        [SkillBrowserEngine::Firefox],
        BTreeMap::from([("viewport".into(), "api_key=secret".into())]),
    )
    .is_err());
    assert!(SkillSessionState::new(
        SessionId::new(),
        BTreeMap::from([("SkillGhost".into(), "ghp_secret".into())]),
        None,
        None,
        None,
        None,
        None,
        Vec::new(),
        vec![evidence()],
        Utc::now(),
    )
    .is_err());
}

#[test]
fn field_specific_metadata_grammars_preserve_safe_wire_values() {
    assert!(SkillEvidenceRef::new("capture:step-1.0", "a".repeat(64)).is_ok());
    assert!(SkillProfileRequest::new(
        [SkillCapability::Locale],
        [],
        [SkillBrowserEngine::Firefox],
        BTreeMap::from([
            ("locale".into(), "en-US".into()),
            ("timezone".into(), "America/New_York".into()),
            (
                "userAgentConsistency".into(),
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36".into()
            ),
            ("engineSelection".into(), "firefox-128.0".into()),
        ]),
    )
    .is_ok());
    assert!(SkillSessionState::new(
        SessionId::new(),
        BTreeMap::from([("SkillGhost".into(), "v1.2.3-beta+1".into())]),
        None,
        None,
        None,
        None,
        None,
        Vec::new(),
        vec![evidence()],
        Utc::now(),
    )
    .is_ok());
}

#[test]
fn checkpoint_proofs_bind_the_session_are_fresh_and_fail_closed_when_absent() {
    let session_id = SessionId::new();
    let checkpoint_id = types::CheckpointId::new();
    let proof = types::SkillCheckpointProof::new(
        checkpoint_id.clone(),
        session_id.clone(),
        Utc::now(),
        types::SkillEvidenceRef::new("checkpoint-attestation", "c".repeat(64)).unwrap(),
    )
    .unwrap();
    let state = SkillSessionState::new(
        session_id.clone(),
        BTreeMap::new(),
        None,
        Some(checkpoint_id.clone()),
        Some(proof),
        None,
        None,
        Vec::new(),
        Vec::new(),
        Utc::now() + chrono::Duration::minutes(5),
    )
    .unwrap();
    assert_eq!(
        serde_json::from_value::<SkillSessionState>(serde_json::to_value(&state).unwrap()).unwrap(),
        state
    );

    let mismatched_session = types::SkillCheckpointProof::new(
        checkpoint_id.clone(),
        SessionId::new(),
        Utc::now(),
        types::SkillEvidenceRef::new("checkpoint-attestation", "c".repeat(64)).unwrap(),
    )
    .unwrap();
    assert!(SkillSessionState::new(
        session_id.clone(),
        BTreeMap::new(),
        None,
        Some(checkpoint_id.clone()),
        Some(mismatched_session),
        None,
        None,
        Vec::new(),
        Vec::new(),
        Utc::now() + chrono::Duration::minutes(5),
    )
    .is_err());
    let arbitrary_checkpoint = types::SkillCheckpointProof::new(
        types::CheckpointId::new(),
        session_id.clone(),
        Utc::now(),
        types::SkillEvidenceRef::new("checkpoint-attestation", "c".repeat(64)).unwrap(),
    )
    .unwrap();
    assert!(SkillSessionState::new(
        session_id.clone(),
        BTreeMap::new(),
        None,
        Some(checkpoint_id.clone()),
        Some(arbitrary_checkpoint),
        None,
        None,
        Vec::new(),
        Vec::new(),
        Utc::now() + chrono::Duration::minutes(5),
    )
    .is_err());
    assert!(types::SkillCheckpointProof::new(
        checkpoint_id.clone(),
        session_id.clone(),
        Utc::now() - chrono::Duration::hours(1),
        types::SkillEvidenceRef::new("checkpoint-attestation", "c".repeat(64)).unwrap(),
    )
    .is_err());

    let mut legacy = serde_json::to_value(&state).unwrap();
    legacy.as_object_mut().unwrap().remove("verifiedCheckpoint");
    assert!(serde_json::from_value::<SkillSessionState>(legacy).is_err());
    assert!(SkillSessionState::new(
        session_id,
        BTreeMap::new(),
        None,
        Some(types::CheckpointId::new()),
        None,
        None,
        None,
        Vec::new(),
        Vec::new(),
        Utc::now() + chrono::Duration::minutes(5),
    )
    .is_err());
}

#[test]
fn decision_requires_a_selected_engine_only_for_engine_switching() {
    assert!(SkillDecision::new(
        SkillTactic::ObserveAgain,
        SkillFailure::TargetDrift,
        "resolved",
        100,
        100,
        None,
        Some(SkillBrowserEngine::Firefox),
    )
    .is_err());
    assert!(SkillDecision::new(
        SkillTactic::SelectCompatibleEngine,
        SkillFailure::EngineUnavailable,
        "resolved",
        100,
        100,
        None,
        None,
    )
    .is_err());
}

#[test]
fn pending_issuance_requires_an_exact_reservation_and_session_bound_proof() {
    let session_id = SessionId::new();
    let checkpoint_id = types::CheckpointId::new();
    let proof = types::SkillCheckpointProof::new(
        checkpoint_id.clone(),
        session_id.clone(),
        Utc::now(),
        types::SkillEvidenceRef::new("checkpoint-attestation", "f".repeat(64)).unwrap(),
    )
    .unwrap();
    let issued_at = Utc::now();
    let deadline = issued_at + chrono::Duration::milliseconds(100);
    let decision = SkillDecision::new(
        SkillTactic::ReconcileCheckpoint,
        SkillFailure::EffectUncertain,
        "reconciled postcondition",
        100,
        100,
        Some(checkpoint_id.clone()),
        None,
    )
    .unwrap();
    let pending = types::SkillIssuedDecision::new(
        types::CommandId::new(),
        session_id.clone(),
        decision,
        Some(proof.clone()),
        issued_at,
        deadline,
    )
    .unwrap();
    let state = SkillSessionState::new(
        session_id.clone(),
        BTreeMap::new(),
        None,
        Some(checkpoint_id),
        Some(proof),
        Some(SkillTactic::ReconcileCheckpoint),
        Some(pending.clone()),
        vec![SkillTactic::ReconcileCheckpoint],
        Vec::new(),
        deadline,
    )
    .unwrap();
    assert!(serde_json::to_value(&state).is_ok());
    assert_eq!(
        serde_json::from_value::<types::SkillIssuedDecision>(
            serde_json::to_value(&pending).unwrap()
        )
        .unwrap(),
        pending
    );
    let mut malformed_issuance = pending.clone();
    malformed_issuance.deadline = malformed_issuance.issued_at - chrono::Duration::milliseconds(1);
    assert!(serde_json::to_value(&malformed_issuance).is_err());

    let mut inflated_issuance = pending.clone();
    inflated_issuance.decision.remaining_deadline_ms += 1;
    inflated_issuance.decision.tactic_budget_ms += 1;
    assert!(serde_json::to_value(&inflated_issuance).is_err());

    let mut mismatched_deadline = state.clone();
    mismatched_deadline
        .pending_issuance
        .as_mut()
        .unwrap()
        .deadline += chrono::Duration::minutes(1);
    assert!(serde_json::to_value(&mismatched_deadline).is_err());

    let mut mismatched_session = state.clone();
    mismatched_session
        .pending_issuance
        .as_mut()
        .unwrap()
        .session_id = SessionId::new();
    assert!(serde_json::to_value(&mismatched_session).is_err());

    let mut malformed_wire = serde_json::to_value(&state).unwrap();
    malformed_wire["pendingIssuance"]["sessionId"] =
        serde_json::to_value(SessionId::new()).unwrap();
    assert!(serde_json::from_value::<SkillSessionState>(malformed_wire).is_err());

    let mut inflated_wire = serde_json::to_value(&state).unwrap();
    inflated_wire["pendingIssuance"]["decision"]["remainingDeadlineMs"] =
        serde_json::json!(101_u64);
    inflated_wire["pendingIssuance"]["decision"]["tacticBudgetMs"] = serde_json::json!(101_u64);
    assert!(serde_json::from_value::<SkillSessionState>(inflated_wire).is_err());

    assert!(SkillSessionState::new(
        session_id,
        BTreeMap::new(),
        None,
        None,
        None,
        Some(SkillTactic::ObserveAgain),
        None,
        Vec::new(),
        Vec::new(),
        deadline,
    )
    .is_err());
}

#[test]
fn issued_command_identity_fails_closed_if_public_fields_are_tampered() {
    let session_id = SessionId::new();
    let issued_at = Utc::now();
    let deadline = issued_at + chrono::Duration::milliseconds(100);
    let decision = SkillDecision::new(
        SkillTactic::ObserveAgain,
        SkillFailure::TargetDrift,
        "observed postcondition",
        100,
        100,
        None,
        None,
    )
    .unwrap();
    let identity = types::SkillCommandIdentity::new(
        types::CommandId::new(),
        types::WorkflowId::new(),
        types::AttemptId::new(),
        session_id.clone(),
        None,
        types::CommandClass::Replayable,
        "a".repeat(64),
    )
    .unwrap();
    let mut issued = types::SkillIssuedDecision::new_for_command(
        types::CommandId::new(),
        session_id,
        identity,
        decision,
        None,
        issued_at,
        deadline,
    )
    .unwrap();
    issued.command_identity.as_mut().unwrap().command_sha256 = "g".repeat(64);

    assert!(serde_json::to_value(&issued).is_err());
}

#[test]
fn reviewed_bounds_and_embedded_unsafe_display_text_fail_closed() {
    let ua_128 = "M".repeat(128);
    assert!(SkillProfileRequest::new(
        [SkillCapability::UserAgentConsistency],
        [],
        [SkillBrowserEngine::Firefox],
        BTreeMap::from([("userAgentConsistency".into(), ua_128)]),
    )
    .is_ok());
    assert!(SkillProfileRequest::new(
        [SkillCapability::UserAgentConsistency],
        [],
        [SkillBrowserEngine::Firefox],
        BTreeMap::from([("userAgentConsistency".into(), "M".repeat(129))]),
    )
    .is_err());

    assert!(SkillSessionState::new(
        SessionId::new(),
        BTreeMap::from([(format!("Skill{}", "A".repeat(123)), "v1".into())]),
        None,
        None,
        None,
        None,
        None,
        Vec::new(),
        vec![evidence()],
        Utc::now()
    )
    .is_ok());
    assert!(SkillSessionState::new(
        SessionId::new(),
        BTreeMap::from([(format!("Skill{}", "A".repeat(124)), "v1".into())]),
        None,
        None,
        None,
        None,
        None,
        Vec::new(),
        vec![evidence()],
        Utc::now()
    )
    .is_err());

    for postcondition in [
        "resolved with Basic YWxpY2U6c2VjcmV0",
        "resolved from ~/secrets",
    ] {
        assert!(SkillDecision::new(
            SkillTactic::ObserveAgain,
            SkillFailure::TargetDrift,
            postcondition,
            1,
            1,
            None,
            None,
        )
        .is_err());
    }
    assert!(SkillProfile::new(
        "v1",
        SkillBrowserEngine::Firefox,
        [],
        "locale=en-US; ghp_secret"
    )
    .is_err());
    let literal = SkillDecision {
        tactic: SkillTactic::ObserveAgain,
        trigger: SkillFailure::TargetDrift,
        expected_postcondition: "resolved with Basic secret".into(),
        remaining_deadline_ms: 1,
        tactic_budget_ms: 1,
        checkpoint_id: None,
        selected_engine: None,
    };
    assert!(serde_json::to_value(literal).is_err());
}

#[test]
fn display_text_rejects_embedded_secret_terms_and_windows_paths() {
    for text in [
        "resolved with password secret",
        "resolved with cookie: secret",
        r"resolved from C:\secrets",
        "resolved from C:/secrets",
    ] {
        assert!(SkillDecision::new(
            SkillTactic::ObserveAgain,
            SkillFailure::TargetDrift,
            text,
            1,
            1,
            None,
            None,
        )
        .is_err());
    }
    let literal = SkillDecision {
        tactic: SkillTactic::ObserveAgain,
        trigger: SkillFailure::TargetDrift,
        expected_postcondition: "resolved with password secret".into(),
        remaining_deadline_ms: 1,
        tactic_budget_ms: 1,
        checkpoint_id: None,
        selected_engine: None,
    };
    assert!(serde_json::to_value(literal).is_err());
}

#[test]
fn public_outcomes_fail_closed_when_evidence_exceeds_the_wire_limit() {
    assert!(serde_json::to_value(SkillOutcome::Applied {
        evidence: vec![evidence(); 128]
    })
    .is_ok());
    assert!(serde_json::to_value(SkillOutcome::Applied {
        evidence: vec![evidence(); 129]
    })
    .is_err());
    assert!(serde_json::to_value(SkillOutcome::Stopped {
        evidence: vec![evidence(); 129]
    })
    .is_err());
}
