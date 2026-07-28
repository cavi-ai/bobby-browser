use super::*;

pub(super) fn ensure_checkpoint_authority(
    decision: &SkillDecision,
    recovered: &RecoveryDecision,
) -> Result<(), CommandError> {
    let checkpoint_id = match recovered {
        RecoveryDecision::Resumed { checkpoint_id, .. }
        | RecoveryDecision::NeedsReconciliation { checkpoint_id, .. }
        | RecoveryDecision::Restarted { checkpoint_id, .. } => checkpoint_id,
    };
    if decision.checkpoint_id.as_ref() != Some(checkpoint_id) {
        return Err(recovery_error(
            ErrorCode::InvalidRequest,
            "recovery checkpoint does not match the reviewed skill decision",
            false,
        ));
    }
    Ok(())
}

pub(super) fn recovery_coordinator_error(error: crate::RecoveryError) -> CommandError {
    let error = recovery_error(
        ErrorCode::VerificationFailed,
        format!("checkpoint recovery failed: {error}"),
        false,
    );
    if error.message.contains("checkpoint invariants failed")
        || error.message.contains("checkpoint changed after")
    {
        checkpoint_mismatch_error(error.message)
    } else {
        error
    }
}

pub(super) fn checkpoint_mismatch_error(message: impl Into<String>) -> CommandError {
    recovery_error(
        ErrorCode::VerificationFailed,
        format!("checkpoint mismatch: {}", message.into()),
        false,
    )
}

pub(super) fn is_checkpoint_mismatch_error(error: &CommandError) -> bool {
    error.message.starts_with("checkpoint mismatch:")
}
impl SkillRecoveryCoordinator {
    pub(super) async fn ensure_recovery_authority(
        &self,
        decision: &SkillDecision,
        envelope: &CommandEnvelope,
    ) -> Result<VerifiedRecoveryCheckpoint, CommandError> {
        let verified = self
            .recovery
            .lock_verified_checkpoint(&envelope.workflow_id)
            .await
            .map_err(recovery_coordinator_error)?;
        let checkpoint = verified.checkpoint();
        let proof = self
            .skills
            .lock()
            .await
            .session_state()
            .pending_issuance
            .as_ref()
            .filter(|issued| &issued.decision == decision)
            .and_then(|issued| issued.checkpoint_proof.as_ref())
            .cloned()
            .ok_or_else(|| {
                checkpoint_mismatch_error("issued recovery decision has no checkpoint proof")
            })?;
        let page_id = envelope.page_id.as_ref().ok_or_else(|| {
            recovery_error(
                ErrorCode::InvalidRequest,
                "recovery command is missing its page identity",
                false,
            )
        })?;
        let boundary_matches = envelope.command.class() != CommandClass::Boundary
            || (checkpoint.recovery_class == CommandClass::Boundary
                && checkpoint.boundary_command_id.as_ref() == Some(&envelope.command_id));
        if decision.checkpoint_id.as_ref() != Some(&checkpoint.checkpoint_id)
            || checkpoint.workflow_id != envelope.workflow_id
            || checkpoint.attempt_id != envelope.attempt_id
            || checkpoint.session_id != envelope.session_id
            || checkpoint.page_id != *page_id
            || proof.checkpoint_id != checkpoint.checkpoint_id
            || proof.session_id != checkpoint.session_id
            || !proof.is_fresh_at(Utc::now())
            || proof.attestation.sha256 != verified.digest()
            || !boundary_matches
        {
            return Err(checkpoint_mismatch_error(
                "recovery checkpoint does not match the reviewed command authority",
            ));
        }
        verified
            .verify_unchanged()
            .await
            .map_err(recovery_coordinator_error)?;
        Ok(verified)
    }
}
