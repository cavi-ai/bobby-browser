use chrono::Utc;
use types::{
    AttemptId, CheckpointId, CheckpointInvariant, CommandClass, CommandId, Evidence, PageId,
    RecoveryDecision, RestartLineage, SessionId, WorkflowCheckpoint, WorkflowId,
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
