use chrono::Utc;
use types::{
    AttemptId, CheckpointId, CheckpointInvariant, CommandClass, CommandId, CommandOutcome,
    Evidence, PageId, RecoveryCommandIdentity, RecoveryDecision, RecoveryReceipt,
    RecoveryReceiptState, RestartLineage, SessionId, SkillDecision, SkillFailure, SkillOutcome,
    SkillTactic, WorkflowCheckpoint, WorkflowId,
};

fn checkpoint() -> WorkflowCheckpoint {
    WorkflowCheckpoint {
        schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
        checkpoint_id: CheckpointId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: PageId::new(),
        restart_url: "https://example.test/start".into(),
        current_url: "https://example.test/step-two".into(),
        cursor: Some(CommandId::new()),
        boundary_command_id: None,
        recovery_class: CommandClass::Reconciliable,
        invariants: vec![
            CheckpointInvariant::Url {
                value: "https://example.test/step-two".into(),
            },
            CheckpointInvariant::Title {
                value: "Step Two".into(),
            },
            CheckpointInvariant::Text {
                selector: "#name".into(),
                value: "Ada".into(),
            },
        ],
        replayable_inputs: vec!["Ada".into()],
        evidence: vec![Evidence::Navigation {
            url: "https://example.test/step-two".into(),
            title: "Step Two".into(),
        }],
        recovery_history: Vec::new(),
        recovery_receipts: Vec::new(),
        created_at: Utc::now(),
    }
}

#[test]
fn checkpoint_contract_round_trips_with_camel_case_fields() {
    let checkpoint = checkpoint();
    let value = serde_json::to_value(&checkpoint).unwrap();

    assert_eq!(value["schemaVersion"], WorkflowCheckpoint::SCHEMA_VERSION);
    assert_eq!(value["restartUrl"], "https://example.test/start");
    assert_eq!(value["recoveryClass"], "reconciliable");
    assert!(value.get("checkpoint_id").is_none());

    let decoded: WorkflowCheckpoint = serde_json::from_value(value).unwrap();
    assert_eq!(decoded.checkpoint_id, checkpoint.checkpoint_id);
    assert_eq!(decoded.invariants, checkpoint.invariants);
}

#[test]
fn recovery_decisions_preserve_checkpoint_and_attempt_lineage() {
    let checkpoint = checkpoint();
    let resumed = RecoveryDecision::Resumed {
        checkpoint_id: checkpoint.checkpoint_id.clone(),
        attempt_id: checkpoint.attempt_id.clone(),
        evidence: checkpoint.evidence.clone(),
    };
    assert!(matches!(
        resumed,
        RecoveryDecision::Resumed { checkpoint_id, attempt_id, .. }
            if checkpoint_id == checkpoint.checkpoint_id && attempt_id == checkpoint.attempt_id
    ));

    let new_attempt_id = AttemptId::new();
    let restarted = RecoveryDecision::Restarted {
        checkpoint_id: checkpoint.checkpoint_id.clone(),
        lineage: RestartLineage {
            workflow_id: checkpoint.workflow_id.clone(),
            abandoned_attempt_id: checkpoint.attempt_id.clone(),
            attempt_id: new_attempt_id.clone(),
            reason: "invariant mismatch".into(),
        },
        evidence: Vec::new(),
    };
    assert!(matches!(
        restarted,
        RecoveryDecision::Restarted { lineage, .. }
            if lineage.abandoned_attempt_id == checkpoint.attempt_id
                && lineage.attempt_id == new_attempt_id
    ));

    let reconciliation = RecoveryDecision::NeedsReconciliation {
        checkpoint_id: checkpoint.checkpoint_id.clone(),
        attempt_id: checkpoint.attempt_id.clone(),
        reason: "boundary effect is uncertain".into(),
        evidence: Vec::new(),
    };
    assert!(matches!(
        reconciliation,
        RecoveryDecision::NeedsReconciliation { checkpoint_id, .. }
            if checkpoint_id == checkpoint.checkpoint_id
    ));
}

#[test]
fn restarted_decision_deserializes_pre_evidence_checkpoint_shape() {
    let mut value = serde_json::to_value(RecoveryDecision::Restarted {
        checkpoint_id: CheckpointId::new(),
        lineage: RestartLineage {
            workflow_id: WorkflowId::new(),
            abandoned_attempt_id: AttemptId::new(),
            attempt_id: AttemptId::new(),
            reason: "legacy restart".into(),
        },
        evidence: Vec::new(),
    })
    .unwrap();
    value.as_object_mut().unwrap().remove("evidence");

    let decoded: RecoveryDecision = serde_json::from_value(value).unwrap();
    assert!(matches!(
        decoded,
        RecoveryDecision::Restarted { evidence, .. } if evidence.is_empty()
    ));
}

#[test]
fn recovery_receipt_round_trips_full_command_identity_and_outbox_state() {
    let identity = RecoveryCommandIdentity::new(
        CommandId::new(),
        WorkflowId::new(),
        AttemptId::new(),
        SessionId::new(),
        Some(PageId::new()),
        CommandClass::Replayable,
        "a".repeat(64),
    )
    .unwrap();
    let receipt = RecoveryReceipt::new(
        identity.command_id.clone(),
        identity.clone(),
        RecoveryReceiptState::PendingJournal,
        CommandId::new(),
        SkillDecision::new(
            SkillTactic::ObserveAgain,
            SkillFailure::TargetDrift,
            "observed postcondition",
            100,
            100,
            None,
            None,
        )
        .unwrap(),
        CommandOutcome::Completed {
            command_id: identity.command_id.clone(),
            evidence: vec![Evidence::Configuration {
                name: "skillRecoveryTactic".into(),
                value: "receipt".into(),
            }],
        },
        SkillOutcome::applied(Vec::new()).unwrap(),
        Vec::new(),
        Utc::now(),
    )
    .unwrap();

    let value = serde_json::to_value(&receipt).unwrap();
    let decoded: RecoveryReceipt = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(decoded.identity, identity);
    assert_eq!(decoded.state, RecoveryReceiptState::PendingJournal);
    assert_eq!(serde_json::to_value(decoded).unwrap(), value);
}
