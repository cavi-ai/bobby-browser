use std::collections::BTreeMap;

use chrono::{Duration, Utc};
use skill_runtime::{
    SkillBrowserEngine, SkillDecision, SkillFailure, SkillSessionState, SkillTactic, SkillTrigger,
    SkillZigZagZig,
};
use types::{
    AttemptId, CommandClass, CommandId, CommandOutcome, RecoveryCommandIdentity, RecoveryReceipt,
    RecoveryReceiptState, SessionId, SkillCommandIdentity, WorkflowId,
};

fn state_with_budget(attempted_tactics: Vec<SkillTactic>) -> SkillSessionState {
    SkillSessionState::new(
        SessionId::new(),
        BTreeMap::new(),
        None,
        None,
        None,
        None,
        None,
        attempted_tactics,
        Vec::new(),
        Utc::now() + Duration::minutes(5),
    )
    .unwrap()
}

fn trigger(failure: SkillFailure) -> SkillTrigger {
    SkillTrigger::new(failure, "workflow postcondition holds").unwrap()
}

fn attach_verified_checkpoint(state: &mut SkillSessionState) {
    let checkpoint_id = types::CheckpointId::new();
    state.last_checkpoint_id = Some(checkpoint_id.clone());
    state.verified_checkpoint = Some(
        types::SkillCheckpointProof::new(
            checkpoint_id,
            state.session_id.clone(),
            Utc::now(),
            types::SkillEvidenceRef::new("checkpoint-attestation", "a".repeat(64)).unwrap(),
        )
        .unwrap(),
    );
}

fn tactics(engine: &SkillZigZagZig, trigger: &SkillTrigger) -> Vec<SkillTactic> {
    let mut state = engine.session_state().clone();
    let mut result = Vec::new();
    loop {
        let mut strategy = SkillZigZagZig::new(
            state.clone(),
            engine.per_tactic_budget_ms(),
            engine.compatible_engines().iter().copied(),
        )
        .unwrap();
        match strategy.next_decision(trigger, Utc::now()) {
            Ok(decision) => {
                result.push(decision.tactic);
                state.attempted_tactics.push(decision.tactic);
            }
            Err(SkillFailure::StrategyExhausted) => return result,
            Err(error) => panic!("unexpected terminal failure: {error:?}"),
        }
    }
}

#[test]
fn ladder_is_ordered_bounded_and_never_replays_uncertain_mutation() {
    let mut state = state_with_budget(Vec::new());
    attach_verified_checkpoint(&mut state);
    state.effective_profile =
        Some(types::SkillProfile::new("1.0.0", SkillBrowserEngine::Firefox, [], "digest").unwrap());
    let mut engine = SkillZigZagZig::new(state, 1_000, [SkillBrowserEngine::Chromium]).unwrap();
    let expected = vec![
        SkillTactic::ObserveAgain,
        SkillTactic::ResolveSemanticTarget,
        SkillTactic::ChangeInteractionMethod,
        SkillTactic::SolveChallenge,
        SkillTactic::ReconcileCheckpoint,
        SkillTactic::FreshGhostSession,
        SkillTactic::SelectCompatibleEngine,
        SkillTactic::RestartDurableBoundary,
    ];

    assert_eq!(
        tactics(&engine, &trigger(SkillFailure::TargetDrift)),
        expected
    );
    assert_eq!(
        engine
            .next_decision(&trigger(SkillFailure::EffectUncertain), Utc::now())
            .unwrap()
            .tactic,
        SkillTactic::ReconcileCheckpoint
    );
}

#[test]
fn decisions_are_limited_by_the_workflow_deadline_and_tactic_budget() {
    let deadline = Utc::now() + Duration::milliseconds(400);
    let mut state = state_with_budget(Vec::new());
    state.deadline = deadline;
    let mut engine = SkillZigZagZig::new(state, 1_000, [SkillBrowserEngine::Firefox]).unwrap();

    let decision = engine
        .next_decision(&trigger(SkillFailure::TargetDrift), Utc::now())
        .unwrap();
    assert!(decision.remaining_deadline_ms <= 400);
    assert_eq!(decision.tactic_budget_ms, decision.remaining_deadline_ms);

    let mut expired = SkillZigZagZig::new(
        state_with_budget(Vec::new()),
        1_000,
        [SkillBrowserEngine::Firefox],
    )
    .unwrap();
    assert_eq!(
        expired.next_decision(
            &trigger(SkillFailure::TargetDrift),
            Utc::now() + Duration::hours(1)
        ),
        Err(SkillFailure::DeadlineExceeded)
    );

    let mut no_tactic_budget = SkillZigZagZig::new(
        state_with_budget(Vec::new()),
        0,
        [SkillBrowserEngine::Firefox],
    )
    .unwrap();
    assert_eq!(
        no_tactic_budget.next_decision(&trigger(SkillFailure::TargetDrift), Utc::now()),
        Err(SkillFailure::StrategyExhausted)
    );
}

#[test]
fn missing_checkpoint_and_reconciliation_exhaustion_preserve_uncertain_terminal_outcome() {
    let mut engine = SkillZigZagZig::new(
        state_with_budget(Vec::new()),
        1_000,
        [SkillBrowserEngine::Firefox],
    )
    .unwrap();
    assert_eq!(
        engine.next_decision(&trigger(SkillFailure::EffectUncertain), Utc::now()),
        Err(SkillFailure::EffectUncertain)
    );

    let mut reconciled = state_with_budget(vec![SkillTactic::ReconcileCheckpoint]);
    attach_verified_checkpoint(&mut reconciled);
    let mut engine = SkillZigZagZig::new(reconciled, 1_000, [SkillBrowserEngine::Firefox]).unwrap();
    assert_eq!(
        engine.next_decision(&trigger(SkillFailure::EffectUncertain), Utc::now()),
        Err(SkillFailure::EffectUncertain)
    );
}

#[test]
fn restart_tactics_require_a_checkpoint_and_engine_selection_requires_an_alternate() {
    let mut state = state_with_budget(vec![
        SkillTactic::ObserveAgain,
        SkillTactic::ResolveSemanticTarget,
        SkillTactic::ChangeInteractionMethod,
        SkillTactic::SolveChallenge,
    ]);
    state.effective_profile =
        Some(types::SkillProfile::new("1.0.0", SkillBrowserEngine::Firefox, [], "digest").unwrap());
    let mut engine = SkillZigZagZig::new(state, 1_000, [SkillBrowserEngine::Firefox]).unwrap();
    assert_eq!(
        engine.next_decision(&trigger(SkillFailure::TargetDrift), Utc::now()),
        Err(SkillFailure::StrategyExhausted)
    );

    let mut state = state_with_budget(vec![
        SkillTactic::ObserveAgain,
        SkillTactic::ResolveSemanticTarget,
        SkillTactic::ChangeInteractionMethod,
        SkillTactic::SolveChallenge,
        SkillTactic::ReconcileCheckpoint,
        SkillTactic::FreshGhostSession,
    ]);
    attach_verified_checkpoint(&mut state);
    state.effective_profile =
        Some(types::SkillProfile::new("1.0.0", SkillBrowserEngine::Firefox, [], "digest").unwrap());
    let mut engine = SkillZigZagZig::new(state, 1_000, [SkillBrowserEngine::Firefox]).unwrap();
    assert_eq!(
        engine
            .next_decision(&trigger(SkillFailure::TargetDrift), Utc::now())
            .unwrap()
            .tactic,
        SkillTactic::RestartDurableBoundary
    );
}

#[test]
fn record_outcome_keeps_evidence_order_deterministic_and_stop_allows_cleanup_only() {
    let state = state_with_budget(Vec::new());
    let mut engine = SkillZigZagZig::new(state, 1_000, [SkillBrowserEngine::Firefox]).unwrap();
    let decision: SkillDecision = engine
        .next_decision(&trigger(SkillFailure::TargetDrift), Utc::now())
        .unwrap();
    engine.stop();
    let outcome = types::SkillOutcome::applied(vec![
        types::SkillEvidenceRef::new("z-last", "b".repeat(64)).unwrap(),
        types::SkillEvidenceRef::new("a-first", "a".repeat(64)).unwrap(),
    ])
    .unwrap();
    engine
        .record_outcome(&decision, &outcome, Utc::now())
        .unwrap();

    assert_eq!(
        engine.session_state().attempted_tactics,
        vec![SkillTactic::ObserveAgain]
    );
    assert_eq!(
        engine
            .session_state()
            .evidence
            .iter()
            .map(|evidence| evidence.artifact_id.as_str())
            .collect::<Vec<_>>(),
        vec!["a-first", "z-last"]
    );
    assert_eq!(
        engine.next_decision(&trigger(SkillFailure::TargetDrift), Utc::now()),
        Err(SkillFailure::StrategyExhausted)
    );
}

#[test]
fn record_outcome_rejects_tampered_checkpoint_metadata_without_mutating_state() {
    let mut state = state_with_budget(vec![
        SkillTactic::ObserveAgain,
        SkillTactic::ResolveSemanticTarget,
        SkillTactic::ChangeInteractionMethod,
        SkillTactic::SolveChallenge,
    ]);
    attach_verified_checkpoint(&mut state);
    let mut engine =
        SkillZigZagZig::new(state.clone(), 1_000, [SkillBrowserEngine::Firefox]).unwrap();
    let mut decision = engine
        .next_decision(&trigger(SkillFailure::TargetDrift), Utc::now())
        .unwrap();
    assert_eq!(decision.tactic, SkillTactic::ReconcileCheckpoint);
    let reserved = engine.session_state().clone();
    decision.checkpoint_id = None;

    assert_eq!(
        engine.record_outcome(
            &decision,
            &types::SkillOutcome::applied(Vec::new()).unwrap(),
            Utc::now(),
        ),
        Err(SkillFailure::ConfigurationConflict)
    );
    assert_eq!(engine.session_state(), &reserved);
}

#[test]
fn issued_decisions_are_reserved_exactly_once_and_expire_after_the_deadline() {
    let mut state = state_with_budget(Vec::new());
    let checkpoint_id = types::CheckpointId::new();
    state.last_checkpoint_id = Some(checkpoint_id.clone());
    state.verified_checkpoint = Some(
        types::SkillCheckpointProof::new(
            checkpoint_id,
            state.session_id.clone(),
            Utc::now(),
            types::SkillEvidenceRef::new("checkpoint-attestation", "d".repeat(64)).unwrap(),
        )
        .unwrap(),
    );
    let mut engine =
        SkillZigZagZig::new(state.clone(), 1_000, [SkillBrowserEngine::Firefox]).unwrap();
    let issued = engine
        .next_decision(&trigger(SkillFailure::TargetDrift), Utc::now())
        .unwrap();
    assert_eq!(
        engine.session_state().attempted_tactics,
        vec![issued.tactic]
    );
    assert_eq!(
        engine.next_decision(&trigger(SkillFailure::TargetDrift), Utc::now()),
        Err(SkillFailure::StrategyExhausted)
    );

    let mut tampered = issued.clone();
    tampered.expected_postcondition = "different postcondition".into();
    assert_eq!(
        engine.record_outcome(
            &tampered,
            &types::SkillOutcome::applied(Vec::new()).unwrap(),
            Utc::now(),
        ),
        Err(SkillFailure::ConfigurationConflict)
    );
    assert_eq!(engine.session_state().evidence, state.evidence);

    for tampered in [
        {
            let mut decision = issued.clone();
            decision.trigger = SkillFailure::PostconditionFailed;
            decision
        },
        {
            let mut decision = issued.clone();
            decision.remaining_deadline_ms -= 1;
            decision
        },
        {
            let mut decision = issued.clone();
            decision.tactic_budget_ms -= 1;
            decision
        },
    ] {
        assert_eq!(
            engine.record_outcome(
                &tampered,
                &types::SkillOutcome::applied(Vec::new()).unwrap(),
                Utc::now(),
            ),
            Err(SkillFailure::ConfigurationConflict)
        );
        assert_eq!(engine.session_state().evidence, state.evidence);
    }

    assert!(engine
        .record_outcome(
            &issued,
            &types::SkillOutcome::applied(Vec::new()).unwrap(),
            engine.session_state().deadline,
        )
        .is_ok());
    assert_eq!(
        engine.record_outcome(
            &issued,
            &types::SkillOutcome::applied(Vec::new()).unwrap(),
            engine.session_state().deadline,
        ),
        Err(SkillFailure::ConfigurationConflict)
    );

    let mut expired = SkillZigZagZig::new(state, 1_000, [SkillBrowserEngine::Firefox]).unwrap();
    let decision = expired
        .next_decision(&trigger(SkillFailure::TargetDrift), Utc::now())
        .unwrap();
    assert_eq!(
        expired.record_outcome(
            &decision,
            &types::SkillOutcome::applied(Vec::new()).unwrap(),
            expired.session_state().deadline + Duration::milliseconds(1),
        ),
        Err(SkillFailure::DeadlineExceeded)
    );
}

#[test]
fn terminal_cleanup_outcome_finalizes_once_after_command_deadline() {
    let mut state = state_with_budget(Vec::new());
    state.deadline = Utc::now() + Duration::milliseconds(10);
    let mut engine = SkillZigZagZig::new(state, 10, [SkillBrowserEngine::Firefox]).unwrap();
    let decision = engine
        .next_decision(&trigger(SkillFailure::TargetDrift), Utc::now())
        .unwrap();
    let finalized_at = engine.session_state().deadline + Duration::milliseconds(20);
    let finalization_deadline = finalized_at + Duration::seconds(1);
    let terminal = types::SkillOutcome::failed(SkillFailure::DeadlineExceeded, Vec::new()).unwrap();

    engine
        .record_terminal_outcome(&decision, &terminal, finalized_at, finalization_deadline)
        .unwrap();
    assert!(engine.session_state().pending_issuance.is_none());
    assert_eq!(
        engine.record_terminal_outcome(&decision, &terminal, finalized_at, finalization_deadline),
        Err(SkillFailure::ConfigurationConflict)
    );
}

#[test]
fn engine_selection_is_canonical_and_proof_bound() {
    let mut state = state_with_budget(vec![
        SkillTactic::ObserveAgain,
        SkillTactic::ResolveSemanticTarget,
        SkillTactic::ChangeInteractionMethod,
        SkillTactic::SolveChallenge,
        SkillTactic::ReconcileCheckpoint,
        SkillTactic::FreshGhostSession,
    ]);
    let checkpoint_id = types::CheckpointId::new();
    state.last_checkpoint_id = Some(checkpoint_id.clone());
    state.verified_checkpoint = Some(
        types::SkillCheckpointProof::new(
            checkpoint_id,
            state.session_id.clone(),
            Utc::now(),
            types::SkillEvidenceRef::new("checkpoint-attestation", "e".repeat(64)).unwrap(),
        )
        .unwrap(),
    );
    state.effective_profile =
        Some(types::SkillProfile::new("1.0.0", SkillBrowserEngine::Firefox, [], "digest").unwrap());
    let mut engine = SkillZigZagZig::new(
        state,
        1_000,
        [
            SkillBrowserEngine::WebKit,
            SkillBrowserEngine::Firefox,
            SkillBrowserEngine::Chromium,
        ],
    )
    .unwrap();
    let decision = engine
        .next_decision(&trigger(SkillFailure::EngineUnavailable), Utc::now())
        .unwrap();
    assert_eq!(decision.tactic, SkillTactic::SelectCompatibleEngine);
    assert_eq!(decision.selected_engine, Some(SkillBrowserEngine::Chromium));

    let mut reordered_state = engine.session_state().clone();
    reordered_state.attempted_tactics.pop();
    reordered_state.reserved_tactic = None;
    reordered_state.pending_issuance = None;
    let mut reordered = SkillZigZagZig::new(
        reordered_state,
        1_000,
        [
            SkillBrowserEngine::Chromium,
            SkillBrowserEngine::WebKit,
            SkillBrowserEngine::Firefox,
        ],
    )
    .unwrap();
    assert_eq!(
        reordered
            .next_decision(&trigger(SkillFailure::EngineUnavailable), Utc::now())
            .unwrap()
            .selected_engine,
        Some(SkillBrowserEngine::Chromium)
    );

    let mut tampered = decision.clone();
    tampered.selected_engine = Some(SkillBrowserEngine::WebKit);
    assert_eq!(
        engine.record_outcome(
            &tampered,
            &types::SkillOutcome::adapted(SkillTactic::SelectCompatibleEngine, Vec::new()).unwrap(),
            Utc::now(),
        ),
        Err(SkillFailure::ConfigurationConflict)
    );
}

#[test]
fn durable_issued_decision_restores_and_completes_exactly_once() {
    let mut state = state_with_budget(Vec::new());
    attach_verified_checkpoint(&mut state);
    let now = Utc::now();
    let mut issuing = SkillZigZagZig::new(state, 1_000, [SkillBrowserEngine::Firefox]).unwrap();
    let decision = issuing
        .next_decision(&trigger(SkillFailure::TargetDrift), now)
        .unwrap();
    let persisted: SkillSessionState =
        serde_json::from_value(serde_json::to_value(issuing.session_state()).unwrap()).unwrap();

    let mut restored =
        SkillZigZagZig::new(persisted, 1_000, [SkillBrowserEngine::Firefox]).unwrap();
    let reserved = restored.session_state().clone();
    let issued_at = reserved.pending_issuance.as_ref().unwrap().issued_at;
    assert_eq!(
        restored.record_outcome(
            &decision,
            &types::SkillOutcome::applied(Vec::new()).unwrap(),
            issued_at - Duration::milliseconds(1),
        ),
        Err(SkillFailure::ConfigurationConflict)
    );
    assert_eq!(restored.session_state(), &reserved);
    let mut tampered = decision.clone();
    tampered.expected_postcondition = "tampered postcondition".into();
    assert_eq!(
        restored.record_outcome(
            &tampered,
            &types::SkillOutcome::applied(Vec::new()).unwrap(),
            now,
        ),
        Err(SkillFailure::ConfigurationConflict)
    );
    assert_eq!(restored.session_state(), &reserved);
    assert_eq!(
        restored.record_outcome(
            &decision,
            &types::SkillOutcome::applied(Vec::new()).unwrap(),
            restored.session_state().deadline + Duration::milliseconds(1),
        ),
        Err(SkillFailure::DeadlineExceeded)
    );
    assert_eq!(restored.session_state(), &reserved);
    restored.stop();
    restored
        .record_outcome(
            &decision,
            &types::SkillOutcome::applied(Vec::new()).unwrap(),
            now,
        )
        .unwrap();
    assert!(restored.session_state().pending_issuance.is_none());
    assert!(restored.session_state().reserved_tactic.is_none());
    assert_eq!(
        restored.record_outcome(
            &decision,
            &types::SkillOutcome::applied(Vec::new()).unwrap(),
            now,
        ),
        Err(SkillFailure::ConfigurationConflict)
    );

    let mut issuance_without_reservation = reserved.clone();
    issuance_without_reservation.reserved_tactic = None;
    assert!(matches!(
        SkillZigZagZig::new(
            issuance_without_reservation,
            1_000,
            [SkillBrowserEngine::Firefox]
        ),
        Err(SkillFailure::ConfigurationConflict)
    ));

    let mut reservation_without_issuance = reserved.clone();
    reservation_without_issuance.pending_issuance = None;
    assert!(matches!(
        SkillZigZagZig::new(
            reservation_without_issuance,
            1_000,
            [SkillBrowserEngine::Firefox]
        ),
        Err(SkillFailure::ConfigurationConflict)
    ));

    let mut expired_state = restored.session_state().clone();
    let expired_deadline = now - Duration::milliseconds(1);
    let mut expired_decision = decision.clone();
    expired_decision.remaining_deadline_ms = 1;
    expired_decision.tactic_budget_ms = 1;
    expired_state.deadline = expired_deadline;
    expired_state.reserved_tactic = Some(SkillTactic::ObserveAgain);
    expired_state.pending_issuance = Some(
        types::SkillIssuedDecision::new(
            types::CommandId::new(),
            expired_state.session_id.clone(),
            expired_decision,
            None,
            now - Duration::milliseconds(2),
            expired_deadline,
        )
        .unwrap(),
    );
    assert!(matches!(
        SkillZigZagZig::new(expired_state, 1_000, [SkillBrowserEngine::Firefox]),
        Err(SkillFailure::DeadlineExceeded)
    ));
}

#[test]
fn committed_receipt_settlement_requires_the_exact_durable_issuance() {
    let state = state_with_budget(Vec::new());
    let session_id = state.session_id.clone();
    let now = Utc::now();
    let command_id = CommandId::new();
    let workflow_id = WorkflowId::new();
    let attempt_id = AttemptId::new();
    let command_sha256 = "c".repeat(64);
    let command_identity = SkillCommandIdentity::new(
        command_id.clone(),
        workflow_id.clone(),
        attempt_id.clone(),
        session_id.clone(),
        None,
        CommandClass::Replayable,
        command_sha256.clone(),
    )
    .unwrap();
    let mut issuing = SkillZigZagZig::new(state, 1_000, [SkillBrowserEngine::Firefox]).unwrap();
    let decision = issuing
        .next_decision_for_command(&trigger(SkillFailure::TargetDrift), command_identity, now)
        .unwrap();
    let issued = issuing.session_state().pending_issuance.as_ref().unwrap();
    let receipt = RecoveryReceipt::new(
        command_id.clone(),
        RecoveryCommandIdentity::new(
            command_id.clone(),
            workflow_id,
            attempt_id,
            session_id,
            None,
            CommandClass::Replayable,
            command_sha256,
        )
        .unwrap(),
        RecoveryReceiptState::Committed,
        issued.reservation_id.clone(),
        decision.clone(),
        CommandOutcome::Completed {
            command_id,
            evidence: Vec::new(),
        },
        types::SkillOutcome::adapted(decision.tactic, Vec::new()).unwrap(),
        Vec::new(),
        now,
    )
    .unwrap();

    for mismatched in [
        {
            let mut receipt = receipt.clone();
            receipt.identity.command_sha256 = "d".repeat(64);
            receipt
        },
        {
            let mut receipt = receipt.clone();
            receipt.reservation_id = CommandId::new();
            receipt
        },
        {
            let mut receipt = receipt.clone();
            receipt.decision.expected_postcondition = "different postcondition".into();
            receipt
        },
        {
            let mut receipt = receipt.clone();
            receipt.outcome_sha256 = "e".repeat(64);
            receipt
        },
    ] {
        let before = issuing.session_state().clone();
        assert_eq!(
            issuing.settle_committed_receipt(&mismatched),
            Err(SkillFailure::ConfigurationConflict)
        );
        assert_eq!(issuing.session_state(), &before);
    }

    issuing.settle_committed_receipt(&receipt).unwrap();
    assert!(issuing.session_state().pending_issuance.is_none());
    issuing.settle_committed_receipt(&receipt).unwrap();
}
