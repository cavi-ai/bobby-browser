use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::Mutex;
use types::{
    CommandId, RecoveryReceipt, RecoveryReceiptState, SkillBrowserEngine, SkillCapability,
    SkillCheckpointProof, SkillCommand, SkillCommandIdentity, SkillDecision, SkillEvidenceRef,
    SkillFailure, SkillIssuedDecision, SkillOutcome, SkillSessionState, SkillTactic,
    SkillZigZagZigCommand,
};

use crate::{Skill, SkillContext, SkillStateStore, SkillStateStoreError};

const LADDER: [SkillTactic; 7] = [
    SkillTactic::ObserveAgain,
    SkillTactic::ResolveSemanticTarget,
    SkillTactic::ChangeInteractionMethod,
    SkillTactic::ReconcileCheckpoint,
    SkillTactic::FreshGhostSession,
    SkillTactic::SelectCompatibleEngine,
    SkillTactic::RestartDurableBoundary,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillTrigger {
    pub failure: SkillFailure,
    pub expected_postcondition: String,
}

impl SkillTrigger {
    pub fn new(
        failure: SkillFailure,
        expected_postcondition: impl Into<String>,
    ) -> Result<Self, SkillFailure> {
        let trigger = Self {
            failure,
            expected_postcondition: expected_postcondition.into(),
        };
        SkillDecision::new(
            SkillTactic::ObserveAgain,
            trigger.failure,
            &trigger.expected_postcondition,
            1,
            1,
            None,
            None,
        )
        .map_err(|_| SkillFailure::ConfigurationConflict)?;
        Ok(trigger)
    }
}

/// A deterministic recovery-strategy selector for one workflow session.
///
/// Issuing a decision atomically reserves its tactic in the local durable-state copy. The caller
/// persists that reserved state through `SkillStateStore::transition` before executing the tactic.
#[derive(Debug, Clone)]
pub struct SkillZigZagZig {
    state: SkillSessionState,
    per_tactic_budget_ms: u64,
    compatible_engines: Vec<SkillBrowserEngine>,
    stopped: bool,
}

pub struct SkillZigZagZigController {
    store: Arc<SkillStateStore>,
    per_tactic_budget_ms: u64,
    compatible_engines: Vec<SkillBrowserEngine>,
    live: Mutex<HashMap<types::SessionId, SkillZigZagZig>>,
}

impl SkillZigZagZigController {
    pub fn new(
        store: Arc<SkillStateStore>,
        per_tactic_budget_ms: u64,
        compatible_engines: impl IntoIterator<Item = SkillBrowserEngine>,
    ) -> Self {
        Self {
            store,
            per_tactic_budget_ms,
            compatible_engines: compatible_engines.into_iter().collect(),
            live: Mutex::new(HashMap::new()),
        }
    }

    pub async fn strategy(
        &self,
        session_id: &types::SessionId,
    ) -> Result<SkillZigZagZig, SkillFailure> {
        self.live
            .lock()
            .await
            .get(session_id)
            .cloned()
            .ok_or(SkillFailure::ConfigurationConflict)
    }

    async fn run(&self, context: &SkillContext) -> Result<SkillOutcome, SkillFailure> {
        let session_id = context
            .session_id()
            .ok_or(SkillFailure::ConfigurationConflict)?;
        let mut live = self.live.lock().await;
        if live.contains_key(session_id) {
            return SkillOutcome::applied(Vec::new())
                .map_err(|_| SkillFailure::ConfigurationConflict);
        }
        self.store
            .transition(session_id, |state| {
                state
                    .active_versions
                    .insert(SkillZigZagZig::NAME.into(), SkillZigZagZig::VERSION.into());
                Ok(())
            })
            .map_err(store_failure)?;
        let state = self.store.get(session_id).map_err(store_failure)?;
        let strategy = SkillZigZagZig::new(
            state,
            self.per_tactic_budget_ms,
            self.compatible_engines.clone(),
        )?;
        live.insert(session_id.clone(), strategy);
        SkillOutcome::applied(Vec::new()).map_err(|_| SkillFailure::ConfigurationConflict)
    }

    async fn stop(&self, context: &SkillContext) -> Result<SkillOutcome, SkillFailure> {
        let session_id = context
            .session_id()
            .ok_or(SkillFailure::ConfigurationConflict)?;
        let mut live = self.live.lock().await;
        let mut strategy = live
            .remove(session_id)
            .ok_or(SkillFailure::ConfigurationConflict)?;
        strategy.stop();
        self.store
            .transition(session_id, |state| {
                state.active_versions.remove(SkillZigZagZig::NAME);
                Ok(())
            })
            .map_err(store_failure)?;
        SkillOutcome::stopped(Vec::new()).map_err(|_| SkillFailure::ConfigurationConflict)
    }

    async fn status(&self, context: &SkillContext) -> Result<SkillOutcome, SkillFailure> {
        let session_id = context
            .session_id()
            .ok_or(SkillFailure::ConfigurationConflict)?;
        if self.live.lock().await.contains_key(session_id) {
            SkillOutcome::applied(Vec::new()).map_err(|_| SkillFailure::ConfigurationConflict)
        } else {
            SkillOutcome::stopped(Vec::new()).map_err(|_| SkillFailure::ConfigurationConflict)
        }
    }
}

#[async_trait]
impl Skill for SkillZigZagZigController {
    fn name(&self) -> &'static str {
        SkillZigZagZig::NAME
    }

    fn alias(&self) -> &'static str {
        SkillZigZagZig::ALIAS
    }

    fn version(&self) -> &'static str {
        SkillZigZagZig::VERSION
    }

    fn capabilities(&self) -> BTreeSet<SkillCapability> {
        BTreeSet::from([
            SkillCapability::EngineSelection,
            SkillCapability::ProfilePersistence,
        ])
    }

    async fn execute(
        &self,
        command: SkillCommand,
        context: &SkillContext,
    ) -> Result<SkillOutcome, SkillFailure> {
        match command {
            SkillCommand::ZigZagZig(SkillZigZagZigCommand::Run) => self.run(context).await,
            SkillCommand::ZigZagZig(SkillZigZagZigCommand::Status) => self.status(context).await,
            SkillCommand::ZigZagZig(SkillZigZagZigCommand::Stop) => self.stop(context).await,
            SkillCommand::Ghost(_) => Err(SkillFailure::ConfigurationConflict),
        }
    }
}

fn store_failure(_error: SkillStateStoreError) -> SkillFailure {
    SkillFailure::ConfigurationConflict
}

impl SkillZigZagZig {
    pub const NAME: &'static str = "SkillZigZagZig";
    pub const ALIAS: &'static str = "/zigzagzig";
    pub const VERSION: &'static str = "1.0.0";

    pub fn new(
        state: SkillSessionState,
        per_tactic_budget_ms: u64,
        compatible_engines: impl IntoIterator<Item = SkillBrowserEngine>,
    ) -> Result<Self, SkillFailure> {
        serde_json::to_vec(&state).map_err(|_| SkillFailure::ConfigurationConflict)?;
        if state
            .pending_issuance
            .as_ref()
            .is_some_and(|issued| !issued.is_active_at(Utc::now()))
        {
            return Err(SkillFailure::DeadlineExceeded);
        }
        let mut compatible_engines: Vec<_> = compatible_engines.into_iter().collect();
        compatible_engines.sort_by_key(|engine| engine_rank(*engine));
        compatible_engines.dedup();
        Ok(Self {
            state,
            per_tactic_budget_ms,
            compatible_engines,
            stopped: false,
        })
    }

    pub fn session_state(&self) -> &SkillSessionState {
        &self.state
    }

    pub fn per_tactic_budget_ms(&self) -> u64 {
        self.per_tactic_budget_ms
    }

    pub fn compatible_engines(&self) -> &[SkillBrowserEngine] {
        &self.compatible_engines
    }

    pub fn stop(&mut self) {
        self.stopped = true;
    }

    pub fn next_decision(
        &mut self,
        trigger: &SkillTrigger,
        now: DateTime<Utc>,
    ) -> Result<SkillDecision, SkillFailure> {
        self.next_decision_inner(trigger, None, now)
    }

    pub fn next_decision_for_command(
        &mut self,
        trigger: &SkillTrigger,
        command_identity: SkillCommandIdentity,
        now: DateTime<Utc>,
    ) -> Result<SkillDecision, SkillFailure> {
        self.next_decision_inner(trigger, Some(command_identity), now)
    }

    fn next_decision_inner(
        &mut self,
        trigger: &SkillTrigger,
        command_identity: Option<SkillCommandIdentity>,
        now: DateTime<Utc>,
    ) -> Result<SkillDecision, SkillFailure> {
        let remaining_deadline_ms = remaining_deadline_ms(self.state.deadline, now)?;
        if self.stopped || self.state.pending_issuance.is_some() || self.per_tactic_budget_ms == 0 {
            return Err(SkillFailure::StrategyExhausted);
        }

        let tactic = self
            .next_tactic(trigger.failure, now)
            .ok_or_else(|| self.terminal_failure(trigger.failure))?;
        let checkpoint_proof = requires_checkpoint(tactic)
            .then(|| self.verified_checkpoint(now))
            .flatten();
        let selected_engine = (tactic == SkillTactic::SelectCompatibleEngine)
            .then(|| self.select_compatible_engine())
            .flatten();
        let decision = SkillDecision::new(
            tactic,
            trigger.failure,
            &trigger.expected_postcondition,
            remaining_deadline_ms,
            self.per_tactic_budget_ms.min(remaining_deadline_ms),
            checkpoint_proof
                .as_ref()
                .map(|proof| proof.checkpoint_id.clone()),
            selected_engine,
        )
        .map_err(|_| SkillFailure::ConfigurationConflict)?;

        let reservation_id = CommandId::new();
        let issued = match command_identity {
            Some(identity) => SkillIssuedDecision::new_for_command(
                reservation_id,
                self.state.session_id.clone(),
                identity,
                decision.clone(),
                checkpoint_proof,
                now,
                self.state.deadline,
            ),
            None => SkillIssuedDecision::new(
                reservation_id,
                self.state.session_id.clone(),
                decision.clone(),
                checkpoint_proof,
                now,
                self.state.deadline,
            ),
        }
        .map_err(|_| SkillFailure::ConfigurationConflict)?;
        let mut next = self.state.clone();
        next.attempted_tactics.push(tactic);
        next.reserved_tactic = Some(tactic);
        next.pending_issuance = Some(issued);
        serde_json::to_vec(&next).map_err(|_| SkillFailure::ConfigurationConflict)?;
        self.state = next;
        Ok(decision)
    }

    /// Records a completed, previously issued decision. Cleanup after `stop()` remains allowed,
    /// but only through the workflow deadline.
    pub fn record_outcome(
        &mut self,
        decision: &SkillDecision,
        outcome: &SkillOutcome,
        now: DateTime<Utc>,
    ) -> Result<(), SkillFailure> {
        if now > self.state.deadline {
            return Err(SkillFailure::DeadlineExceeded);
        }
        self.record_outcome_inner(decision, outcome, now)
    }

    pub fn record_terminal_outcome(
        &mut self,
        decision: &SkillDecision,
        outcome: &SkillOutcome,
        now: DateTime<Utc>,
        finalization_deadline: DateTime<Utc>,
    ) -> Result<(), SkillFailure> {
        if now <= self.state.deadline
            || now > finalization_deadline
            || !matches!(
                outcome_failure(outcome),
                Some(SkillFailure::DeadlineExceeded | SkillFailure::EffectUncertain)
            )
        {
            return Err(SkillFailure::ConfigurationConflict);
        }
        self.record_outcome_inner(decision, outcome, now)
    }

    fn record_outcome_inner(
        &mut self,
        decision: &SkillDecision,
        outcome: &SkillOutcome,
        now: DateTime<Utc>,
    ) -> Result<(), SkillFailure> {
        let issued = self
            .state
            .pending_issuance
            .as_ref()
            .ok_or(SkillFailure::ConfigurationConflict)?;
        if now < issued.issued_at
            || serde_json::to_vec(decision).is_err()
            || decision != &issued.decision
            || !self.proof_is_unchanged(issued, now)
            || outcome_tactic(outcome).is_some_and(|tactic| tactic != decision.tactic)
        {
            return Err(SkillFailure::ConfigurationConflict);
        }

        let mut next = self.state.clone();
        next.reserved_tactic = None;
        next.pending_issuance = None;
        next.evidence.extend(outcome_evidence(outcome));
        next.evidence.sort_by(|left, right| {
            left.artifact_id
                .cmp(&right.artifact_id)
                .then_with(|| left.sha256.cmp(&right.sha256))
        });
        serde_json::to_vec(&next).map_err(|_| SkillFailure::ConfigurationConflict)?;
        self.state = next;
        Ok(())
    }

    pub fn settle_committed_receipt(
        &mut self,
        receipt: &RecoveryReceipt,
    ) -> Result<(), SkillFailure> {
        settle_committed_receipt_state(&mut self.state, receipt)
    }

    fn next_tactic(&self, failure: SkillFailure, now: DateTime<Utc>) -> Option<SkillTactic> {
        if failure == SkillFailure::EffectUncertain {
            return self
                .eligible(SkillTactic::ReconcileCheckpoint, now)
                .then_some(SkillTactic::ReconcileCheckpoint);
        }
        LADDER
            .into_iter()
            .find(|tactic| self.eligible(*tactic, now))
    }

    fn eligible(&self, tactic: SkillTactic, now: DateTime<Utc>) -> bool {
        !self.state.attempted_tactics.contains(&tactic)
            && (!requires_checkpoint(tactic) || self.verified_checkpoint(now).is_some())
            && (tactic != SkillTactic::SelectCompatibleEngine
                || self.select_compatible_engine().is_some())
    }

    fn verified_checkpoint(&self, now: DateTime<Utc>) -> Option<SkillCheckpointProof> {
        let proof = self.state.verified_checkpoint.as_ref()?;
        (self.state.last_checkpoint_id.as_ref() == Some(&proof.checkpoint_id)
            && proof.session_id == self.state.session_id
            && proof.is_fresh_at(now))
        .then(|| proof.clone())
    }

    fn proof_is_unchanged(&self, issued: &SkillIssuedDecision, now: DateTime<Utc>) -> bool {
        match &issued.checkpoint_proof {
            None => true,
            Some(proof) => self.verified_checkpoint(now).as_ref() == Some(proof),
        }
    }

    fn select_compatible_engine(&self) -> Option<SkillBrowserEngine> {
        let current_engine = self.state.effective_profile.as_ref()?.engine;
        self.compatible_engines
            .iter()
            .copied()
            .find(|engine| *engine != current_engine)
    }

    fn terminal_failure(&self, trigger: SkillFailure) -> SkillFailure {
        if trigger == SkillFailure::EffectUncertain {
            SkillFailure::EffectUncertain
        } else {
            SkillFailure::StrategyExhausted
        }
    }
}

pub(crate) fn settle_committed_receipt_state(
    state: &mut SkillSessionState,
    receipt: &RecoveryReceipt,
) -> Result<(), SkillFailure> {
    receipt
        .validate()
        .map_err(|_| SkillFailure::ConfigurationConflict)?;
    if receipt.state != RecoveryReceiptState::Committed {
        return Err(SkillFailure::ConfigurationConflict);
    }
    let Some(issued) = state.pending_issuance.as_ref() else {
        return Ok(());
    };
    let Some(command_identity) = issued.command_identity.as_ref() else {
        return Err(SkillFailure::ConfigurationConflict);
    };
    let identity_matches = command_identity.command_id == receipt.identity.command_id
        && command_identity.workflow_id == receipt.identity.workflow_id
        && command_identity.attempt_id == receipt.identity.attempt_id
        && command_identity.session_id == receipt.identity.session_id
        && command_identity.page_id == receipt.identity.page_id
        && command_identity.command_class == receipt.identity.command_class
        && command_identity.command_sha256 == receipt.identity.command_sha256;
    if !identity_matches
        || issued.reservation_id != receipt.reservation_id
        || issued.decision != receipt.decision
        || outcome_tactic(&receipt.skill_outcome)
            .is_some_and(|tactic| tactic != receipt.decision.tactic)
    {
        return Err(SkillFailure::ConfigurationConflict);
    }

    state.reserved_tactic = None;
    state.pending_issuance = None;
    state
        .evidence
        .extend(outcome_evidence(&receipt.skill_outcome));
    state.evidence.sort_by(|left, right| {
        left.artifact_id
            .cmp(&right.artifact_id)
            .then_with(|| left.sha256.cmp(&right.sha256))
    });
    Ok(())
}

fn remaining_deadline_ms(deadline: DateTime<Utc>, now: DateTime<Utc>) -> Result<u64, SkillFailure> {
    let remaining = deadline.signed_duration_since(now).num_milliseconds();
    if remaining <= 0 {
        return Err(SkillFailure::DeadlineExceeded);
    }
    Ok(remaining as u64)
}

fn requires_checkpoint(tactic: SkillTactic) -> bool {
    matches!(
        tactic,
        SkillTactic::ReconcileCheckpoint
            | SkillTactic::FreshGhostSession
            | SkillTactic::SelectCompatibleEngine
            | SkillTactic::RestartDurableBoundary
    )
}

fn outcome_tactic(outcome: &SkillOutcome) -> Option<SkillTactic> {
    match outcome {
        SkillOutcome::Adapted { tactic, .. } => Some(*tactic),
        _ => None,
    }
}

fn outcome_failure(outcome: &SkillOutcome) -> Option<SkillFailure> {
    match outcome {
        SkillOutcome::Failed { failure, .. } => Some(*failure),
        _ => None,
    }
}

fn outcome_evidence(outcome: &SkillOutcome) -> Vec<SkillEvidenceRef> {
    match outcome {
        SkillOutcome::Applied { evidence }
        | SkillOutcome::Adapted { evidence, .. }
        | SkillOutcome::Degraded { evidence, .. }
        | SkillOutcome::Stopped { evidence }
        | SkillOutcome::Failed { evidence, .. } => evidence.clone(),
    }
}

fn engine_rank(engine: SkillBrowserEngine) -> u8 {
    match engine {
        SkillBrowserEngine::Firefox => 0,
        SkillBrowserEngine::Chromium => 1,
        SkillBrowserEngine::WebKit => 2,
    }
}
