use super::*;

enum PreparedOwnedTactic {
    Recovery {
        checkpoint: Box<WorkflowCheckpoint>,
        effect: SkillTacticEffect,
    },
    Restart {
        checkpoint: Box<WorkflowCheckpoint>,
        restart_url: String,
    },
}

fn owned_task_error(error: tokio::task::JoinError) -> CommandError {
    recovery_error(
        ErrorCode::Internal,
        format!("owned recovery operation failed: {error}"),
        false,
    )
}

pub(super) fn is_owned_recovery_tactic(tactic: SkillTactic) -> bool {
    matches!(
        tactic,
        SkillTactic::ReconcileCheckpoint
            | SkillTactic::FreshGhostSession
            | SkillTactic::SelectCompatibleEngine
            | SkillTactic::RestartDurableBoundary
    )
}

pub(super) fn recovery_progress(
    decision: RecoveryDecision,
    effect: SkillTacticEffect,
    envelope: &CommandEnvelope,
) -> TacticProgress {
    match decision {
        RecoveryDecision::Resumed { .. } => TacticProgress::Continue(effect),
        RecoveryDecision::NeedsReconciliation { .. } => {
            TacticProgress::EffectUncertain(SkillTacticEffect::ReconciliationRequired)
        }
        RecoveryDecision::Restarted {
            lineage, evidence, ..
        } => TacticProgress::Restarted(
            CommandOutcome::Restarted {
                command_id: envelope.command_id.clone(),
                prior_attempt_id: lineage.abandoned_attempt_id,
                attempt_id: lineage.attempt_id,
                reason: lineage.reason,
                evidence,
            },
            effect,
        ),
    }
}

pub(super) fn engine_preference(engine: SkillBrowserEngine) -> EnginePreference {
    use companion_protocol::BrowserEngine;
    let engine = match engine {
        SkillBrowserEngine::Firefox => BrowserEngine::Firefox,
        SkillBrowserEngine::Chromium => BrowserEngine::Chromium,
        SkillBrowserEngine::WebKit => BrowserEngine::WebKit,
    };
    EnginePreference::Prefer {
        engines: vec![engine],
    }
}

pub(super) fn expected_postcondition(command: &RuntimeCommand) -> &'static str {
    let RuntimeCommand::Primitive(command) = command else {
        return "intent postcondition is observed";
    };
    match command {
        PrimitiveCommand::Navigate(_) => "navigation final URL is observed",
        PrimitiveCommand::Click(_) => "click expected URL is observed",
        PrimitiveCommand::TypeText(_) => "typed value is observed at the original target",
        PrimitiveCommand::Inspect(_) => "inspection evidence is observed",
        PrimitiveCommand::UploadFiles(_) => "upload evidence is observed",
        PrimitiveCommand::OpenPage(_) => "opened page evidence is observed",
        PrimitiveCommand::ListPages(_) => "page list evidence is observed",
        PrimitiveCommand::ClosePage(_) => "page closure evidence is observed",
        PrimitiveCommand::ActivatePage(_) => "activated page evidence is observed",
        PrimitiveCommand::ClickAndWaitForPopup(_) => "popup evidence is observed",
        PrimitiveCommand::ClickAndWaitForDownload(_) | PrimitiveCommand::DownloadUrl(_) => {
            "download evidence is observed"
        }
        PrimitiveCommand::WaitFor(_) => "wait condition is observed",
        PrimitiveCommand::CaptureScreenshot(_) => "screenshot artifact is observed",
        PrimitiveCommand::SetFocusEmulation(_) | PrimitiveCommand::SetEmulatedMedia(_) => {
            "requested page configuration is observed"
        }
        PrimitiveCommand::EvaluateJavaScript(_) => "JavaScript result is observed",
    }
}

pub(super) fn alternate_interaction_envelope(
    envelope: &CommandEnvelope,
) -> Result<CommandEnvelope, CommandError> {
    let mut alternate = envelope.clone();
    match &mut alternate.command {
        RuntimeCommand::Primitive(PrimitiveCommand::Click(command)) => {
            alternate_target(&mut command.selector, &mut command.target)
        }
        RuntimeCommand::Primitive(PrimitiveCommand::TypeText(command)) => {
            alternate_target(&mut command.selector, &mut command.target)
        }
        RuntimeCommand::Primitive(PrimitiveCommand::UploadFiles(command)) => {
            alternate_target(&mut command.selector, &mut command.target)
        }
        RuntimeCommand::Primitive(PrimitiveCommand::ClickAndWaitForPopup(command)) => {
            alternate_target(&mut command.selector, &mut command.target)
        }
        RuntimeCommand::Primitive(PrimitiveCommand::ClickAndWaitForDownload(command)) => {
            alternate_target(&mut command.selector, &mut command.target)
        }
        _ => {}
    }
    if !preserves_postcondition(&envelope.command, &alternate.command) {
        return Err(recovery_error(
            ErrorCode::InvalidRequest,
            "interaction-method change altered the original postcondition",
            false,
        ));
    }
    Ok(alternate)
}

fn alternate_target(selector: &mut String, target: &mut Option<TargetSpec>) {
    match target.take() {
        None => {
            *target = Some(TargetSpec {
                css: Some(selector.clone()),
                ..TargetSpec::default()
            });
        }
        Some(existing) => {
            if let Some(css) = existing.css.clone() {
                *selector = css;
            } else {
                *target = Some(existing);
            }
        }
    }
}

fn preserves_postcondition(original: &RuntimeCommand, alternate: &RuntimeCommand) -> bool {
    match (original, alternate) {
        (
            RuntimeCommand::Primitive(PrimitiveCommand::Click(left)),
            RuntimeCommand::Primitive(PrimitiveCommand::Click(right)),
        ) => left.boundary == right.boundary && left.expected_url == right.expected_url,
        (
            RuntimeCommand::Primitive(PrimitiveCommand::TypeText(left)),
            RuntimeCommand::Primitive(PrimitiveCommand::TypeText(right)),
        ) => left.value == right.value && left.clear_first == right.clear_first,
        (
            RuntimeCommand::Primitive(PrimitiveCommand::UploadFiles(left)),
            RuntimeCommand::Primitive(PrimitiveCommand::UploadFiles(right)),
        ) => left.paths == right.paths,
        (
            RuntimeCommand::Primitive(PrimitiveCommand::ClickAndWaitForPopup(left)),
            RuntimeCommand::Primitive(PrimitiveCommand::ClickAndWaitForPopup(right)),
        ) => left.timeout_ms == right.timeout_ms,
        (
            RuntimeCommand::Primitive(PrimitiveCommand::ClickAndWaitForDownload(left)),
            RuntimeCommand::Primitive(PrimitiveCommand::ClickAndWaitForDownload(right)),
        ) => left.timeout_ms == right.timeout_ms,
        _ => serde_json::to_value(original).ok() == serde_json::to_value(alternate).ok(),
    }
}

pub(super) fn tactic_budget(
    decision: &SkillDecision,
    envelope: &CommandEnvelope,
) -> Result<Duration, CommandError> {
    let total = remaining_duration(envelope.deadline)?;
    let decision_total = Duration::from_millis(decision.remaining_deadline_ms);
    let tactic = Duration::from_millis(decision.tactic_budget_ms);
    let budget = total.min(decision_total).min(tactic);
    if budget.is_zero() {
        Err(deadline_error(
            "skill tactic has no remaining deadline budget",
        ))
    } else {
        Ok(budget)
    }
}

pub(super) fn remaining_duration(
    deadline: chrono::DateTime<Utc>,
) -> Result<Duration, CommandError> {
    let millis = deadline
        .signed_duration_since(Utc::now())
        .num_milliseconds();
    if millis <= 0 {
        Err(deadline_error("command deadline has elapsed"))
    } else {
        Ok(Duration::from_millis(millis as u64))
    }
}
impl SkillRecoveryCoordinator {
    pub(super) async fn execute_tactic(
        &self,
        decision: &SkillDecision,
        envelope: &CommandEnvelope,
        page: &PageState,
    ) -> Result<TacticProgress, CommandError> {
        let mut budget = tactic_budget(decision, envelope)?;
        if decision.tactic == SkillTactic::ReconcileCheckpoint {
            let started = std::time::Instant::now();
            let observed = tokio::time::timeout(budget, self.observe_postcondition(envelope, page))
                .await
                .map_err(|_| {
                    deadline_error("skill tactic exceeded its remaining total deadline")
                })??;
            if observed.0 {
                return Ok(TacticProgress::Completed(
                    observed.1,
                    SkillTacticEffect::PostconditionConfirmed,
                ));
            }
            budget = budget.saturating_sub(started.elapsed());
            if budget.is_zero() {
                return Err(deadline_error(
                    "skill tactic exceeded its remaining total deadline",
                ));
            }
        }
        if !matches!(
            decision.tactic,
            SkillTactic::ReconcileCheckpoint
                | SkillTactic::FreshGhostSession
                | SkillTactic::SelectCompatibleEngine
                | SkillTactic::RestartDurableBoundary
        ) {
            return tokio::time::timeout(
                budget,
                self.execute_tactic_inner(decision, envelope, page, false),
            )
            .await
            .map_err(|_| deadline_error("skill tactic exceeded its remaining total deadline"))?;
        }
        let started = std::time::Instant::now();
        let mut verified = self.ensure_recovery_authority(decision, envelope).await?;
        #[cfg(feature = "test-support")]
        if let Some(observer) = &self.preflight_observer {
            observer.checkpoint_verified().await;
        }
        verified
            .verify_unchanged()
            .await
            .map_err(recovery_coordinator_error)?;
        let checkpoint = verified.checkpoint().clone();
        let coordinator = self.clone();
        let owned_decision = decision.clone();
        let owned_envelope = envelope.clone();
        let mut pool_phase = tokio::spawn(async move {
            let _stabilization = coordinator.stabilization_gate.lock().await;
            coordinator
                .prepare_owned_pool_tactic(&owned_decision, &owned_envelope, checkpoint)
                .await
        });

        let prepared = match tokio::time::timeout(budget, &mut pool_phase).await {
            Ok(joined) => joined.map_err(owned_task_error)??,
            Err(_) => {
                drop(verified);
                return match tokio::time::timeout(RECOVERY_FINALIZATION_BUDGET, &mut pool_phase)
                    .await
                {
                    Ok(joined) => {
                        let _ = joined.map_err(owned_task_error)?;
                        self.finalize_owned_deadline(decision, envelope).await?;
                        Err(deadline_error(format!(
                            "{OWNED_TERMINAL_PREFIX}skill tactic exceeded its remaining total deadline"
                        )))
                    }
                    Err(_) => {
                        self.persist_unresolved_deadline(decision, envelope).await?;
                        Err(deadline_error(format!(
                            "{OWNED_TERMINAL_PREFIX}owned recovery is still stabilizing"
                        )))
                    }
                };
            }
        };

        let remaining = budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            drop(verified);
            self.finalize_owned_deadline(decision, envelope).await?;
            return Err(deadline_error(format!(
                "{OWNED_TERMINAL_PREFIX}skill tactic exceeded its remaining total deadline"
            )));
        }
        match tokio::time::timeout(
            remaining,
            self.finish_owned_tactic(decision, envelope, page, &mut verified, prepared),
        )
        .await
        {
            Ok(progress) => progress,
            Err(_) => {
                drop(verified);
                self.finalize_owned_deadline(decision, envelope).await?;
                Err(deadline_error(format!(
                    "{OWNED_TERMINAL_PREFIX}skill tactic exceeded its remaining total deadline"
                )))
            }
        }
    }

    async fn prepare_owned_pool_tactic(
        &self,
        decision: &SkillDecision,
        envelope: &CommandEnvelope,
        checkpoint: WorkflowCheckpoint,
    ) -> Result<PreparedOwnedTactic, CommandError> {
        match decision.tactic {
            SkillTactic::ReconcileCheckpoint => {
                self.recovery
                    .stabilize_recovery_pool(&checkpoint.session_id, true)
                    .await
                    .map_err(recovery_coordinator_error)?;
                Ok(PreparedOwnedTactic::Recovery {
                    checkpoint: Box::new(checkpoint),
                    effect: SkillTacticEffect::CheckpointResumed,
                })
            }
            SkillTactic::FreshGhostSession => {
                self.workers.release_session(&envelope.session_id).await?;
                Ok(PreparedOwnedTactic::Recovery {
                    checkpoint: Box::new(checkpoint),
                    effect: SkillTacticEffect::SessionReplaced,
                })
            }
            SkillTactic::SelectCompatibleEngine => {
                let engine = decision.selected_engine.ok_or_else(|| {
                    recovery_error(
                        ErrorCode::InvalidRequest,
                        "engine replacement decision omitted its reviewed engine",
                        false,
                    )
                })?;
                let replacement = self
                    .workers
                    .replace_session(&envelope.session_id, &engine_preference(engine))
                    .await;
                self.workers
                    .wait_for_session_stable(&envelope.session_id)
                    .await;
                replacement?;
                Ok(PreparedOwnedTactic::Recovery {
                    checkpoint: Box::new(checkpoint),
                    effect: SkillTacticEffect::EngineReplaced,
                })
            }
            SkillTactic::RestartDurableBoundary => {
                let restart_url = checkpoint.restart_url.clone();
                self.recovery
                    .stabilize_recovery_pool(&checkpoint.session_id, true)
                    .await
                    .map_err(recovery_coordinator_error)?;
                Ok(PreparedOwnedTactic::Restart {
                    checkpoint: Box::new(checkpoint),
                    restart_url,
                })
            }
            _ => unreachable!("only pool-owning recovery tactics use this path"),
        }
    }

    async fn finish_owned_tactic(
        &self,
        decision: &SkillDecision,
        envelope: &CommandEnvelope,
        page: &PageState,
        verified: &mut VerifiedRecoveryCheckpoint,
        prepared: PreparedOwnedTactic,
    ) -> Result<TacticProgress, CommandError> {
        match prepared {
            PreparedOwnedTactic::Recovery { checkpoint, effect } => {
                let prepared = self
                    .recovery
                    .reattach_recovery_pool(*checkpoint)
                    .await
                    .map_err(recovery_coordinator_error)?;
                let recovered = self
                    .recovery
                    .complete_prepared_recovery(verified, prepared)
                    .await
                    .map_err(recovery_coordinator_error)?;
                ensure_checkpoint_authority(decision, &recovered)?;
                if decision.tactic == SkillTactic::ReconcileCheckpoint {
                    self.sync_recovered_page(&recovered, envelope, page).await?;
                    return match recovered {
                        RecoveryDecision::Resumed { .. } => {
                            if envelope.command.class() == CommandClass::Boundary {
                                Ok(TacticProgress::EffectUncertain(effect))
                            } else {
                                Ok(TacticProgress::Continue(effect))
                            }
                        }
                        RecoveryDecision::Restarted {
                            lineage, evidence, ..
                        } => Ok(TacticProgress::Restarted(
                            CommandOutcome::Restarted {
                                command_id: envelope.command_id.clone(),
                                prior_attempt_id: lineage.abandoned_attempt_id,
                                attempt_id: lineage.attempt_id,
                                reason: lineage.reason,
                                evidence,
                            },
                            SkillTacticEffect::DurableBoundaryRestarted,
                        )),
                        RecoveryDecision::NeedsReconciliation { .. } => {
                            Ok(TacticProgress::EffectUncertain(
                                SkillTacticEffect::ReconciliationRequired,
                            ))
                        }
                    };
                }
                self.after_replacement(recovered, effect, envelope, page)
                    .await
            }
            PreparedOwnedTactic::Restart {
                checkpoint,
                restart_url,
            } => {
                let mut restarted = self
                    .recovery
                    .prepare_restart_after_stabilization(*checkpoint)
                    .await
                    .map_err(recovery_coordinator_error)?;
                ensure_checkpoint_authority(decision, &restarted)?;
                let attempt_id = match &restarted {
                    RecoveryDecision::Restarted { lineage, .. } => lineage.attempt_id.clone(),
                    _ => unreachable!("durable restart preparation always returns restart lineage"),
                };
                let mut navigation = envelope.clone();
                navigation.command_id = CommandId::new();
                navigation.attempt_id = attempt_id;
                navigation.command =
                    RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
                        url: restart_url,
                        wait_until: WaitUntil::Interactive,
                        timeout_ms: decision.tactic_budget_ms,
                    }));
                let evidence = match self.runtime.execute(navigation).await {
                    CommandOutcome::Completed { evidence, .. } => evidence,
                    outcome => {
                        return Err(recovery_error(
                            ErrorCode::VerificationFailed,
                            format!("durable restart navigation failed: {outcome:?}"),
                            false,
                        ));
                    }
                };
                if let RecoveryDecision::Restarted {
                    evidence: restart_evidence,
                    ..
                } = &mut restarted
                {
                    *restart_evidence = evidence;
                }
                self.recovery
                    .record_locked_decision(verified, &restarted)
                    .await
                    .map_err(recovery_coordinator_error)?;
                Ok(recovery_progress(
                    restarted,
                    SkillTacticEffect::DurableBoundaryRestarted,
                    envelope,
                ))
            }
        }
    }

    async fn execute_tactic_inner(
        &self,
        decision: &SkillDecision,
        envelope: &CommandEnvelope,
        page: &PageState,
        reconcile_observed: bool,
    ) -> Result<TacticProgress, CommandError> {
        let mut verified_checkpoint = if matches!(
            decision.tactic,
            SkillTactic::ReconcileCheckpoint
                | SkillTactic::FreshGhostSession
                | SkillTactic::SelectCompatibleEngine
                | SkillTactic::RestartDurableBoundary
        ) {
            Some(self.ensure_recovery_authority(decision, envelope).await?)
        } else {
            None
        };
        #[cfg(feature = "test-support")]
        if verified_checkpoint.is_some() {
            if let Some(observer) = &self.preflight_observer {
                observer.checkpoint_verified().await;
            }
        }
        match decision.tactic {
            SkillTactic::ObserveAgain | SkillTactic::ResolveSemanticTarget => {
                let observed = self.observe_postcondition(envelope, page).await?;
                if observed.0 {
                    Ok(TacticProgress::Completed(
                        observed.1,
                        SkillTacticEffect::PostconditionConfirmed,
                    ))
                } else if decision.tactic == SkillTactic::ObserveAgain {
                    Ok(TacticProgress::Continue(SkillTacticEffect::Observed))
                } else {
                    Ok(TacticProgress::Continue(SkillTacticEffect::ReResolved))
                }
            }
            SkillTactic::ChangeInteractionMethod => {
                let alternate = alternate_interaction_envelope(envelope)?;
                let outcome = self.runtime.execute(alternate).await;
                let effect = SkillTacticEffect::CommandRetried;
                if let CommandOutcome::Completed { evidence, .. } = outcome {
                    Ok(TacticProgress::Completed(evidence, effect))
                } else {
                    Ok(TacticProgress::Outcome(outcome, effect))
                }
            }
            SkillTactic::ReconcileCheckpoint => {
                if !reconcile_observed {
                    let observed = self.observe_postcondition(envelope, page).await?;
                    if observed.0 {
                        return Ok(TacticProgress::Completed(
                            observed.1,
                            SkillTacticEffect::PostconditionConfirmed,
                        ));
                    }
                }
                let recovered = self
                    .recovery
                    .recover_locked(
                        verified_checkpoint
                            .as_mut()
                            .expect("recovery tactics always preflight a checkpoint"),
                        true,
                    )
                    .await
                    .map_err(recovery_coordinator_error)?;
                ensure_checkpoint_authority(decision, &recovered)?;
                self.sync_recovered_page(&recovered, envelope, page).await?;
                match recovered {
                    RecoveryDecision::Resumed { .. } => {
                        if envelope.command.class() == CommandClass::Boundary {
                            Ok(TacticProgress::EffectUncertain(
                                SkillTacticEffect::CheckpointResumed,
                            ))
                        } else {
                            Ok(TacticProgress::Continue(
                                SkillTacticEffect::CheckpointResumed,
                            ))
                        }
                    }
                    RecoveryDecision::Restarted {
                        lineage, evidence, ..
                    } => Ok(TacticProgress::Restarted(
                        CommandOutcome::Restarted {
                            command_id: envelope.command_id.clone(),
                            prior_attempt_id: lineage.abandoned_attempt_id,
                            attempt_id: lineage.attempt_id,
                            reason: lineage.reason,
                            evidence,
                        },
                        SkillTacticEffect::DurableBoundaryRestarted,
                    )),
                    RecoveryDecision::NeedsReconciliation { .. } => Ok(
                        TacticProgress::EffectUncertain(SkillTacticEffect::ReconciliationRequired),
                    ),
                }
            }
            SkillTactic::FreshGhostSession => {
                let verified = verified_checkpoint
                    .as_mut()
                    .expect("recovery tactics always preflight a checkpoint");
                verified
                    .verify_unchanged()
                    .await
                    .map_err(recovery_coordinator_error)?;
                self.workers.release_session(&envelope.session_id).await?;
                let recovered = self
                    .recovery
                    .recover_locked(verified, false)
                    .await
                    .map_err(recovery_coordinator_error)?;
                ensure_checkpoint_authority(decision, &recovered)?;
                self.after_replacement(
                    recovered,
                    SkillTacticEffect::SessionReplaced,
                    envelope,
                    page,
                )
                .await
            }
            SkillTactic::SelectCompatibleEngine => {
                let engine = decision.selected_engine.ok_or_else(|| {
                    recovery_error(
                        ErrorCode::InvalidRequest,
                        "engine replacement decision omitted its reviewed engine",
                        false,
                    )
                })?;
                let verified = verified_checkpoint
                    .as_mut()
                    .expect("recovery tactics always preflight a checkpoint");
                verified
                    .verify_unchanged()
                    .await
                    .map_err(recovery_coordinator_error)?;
                self.workers
                    .replace_session(&envelope.session_id, &engine_preference(engine))
                    .await?;
                let recovered = self
                    .recovery
                    .recover_locked(verified, false)
                    .await
                    .map_err(recovery_coordinator_error)?;
                ensure_checkpoint_authority(decision, &recovered)?;
                self.after_replacement(recovered, SkillTacticEffect::EngineReplaced, envelope, page)
                    .await
            }
            SkillTactic::RestartDurableBoundary => {
                let verified = verified_checkpoint
                    .as_mut()
                    .expect("recovery tactics always preflight a checkpoint");
                let restart_url = verified.checkpoint().restart_url.clone();
                let mut restarted = self
                    .recovery
                    .prepare_restart_from_locked_boundary(verified)
                    .await
                    .map_err(recovery_coordinator_error)?;
                ensure_checkpoint_authority(decision, &restarted)?;
                let attempt_id = match &restarted {
                    RecoveryDecision::Restarted { lineage, .. } => lineage.attempt_id.clone(),
                    _ => unreachable!("durable restart preparation always returns restart lineage"),
                };
                let mut navigation = envelope.clone();
                navigation.command_id = CommandId::new();
                navigation.attempt_id = attempt_id;
                navigation.command =
                    RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
                        url: restart_url,
                        wait_until: WaitUntil::Interactive,
                        timeout_ms: decision.tactic_budget_ms,
                    }));
                let evidence = match self.runtime.execute(navigation).await {
                    CommandOutcome::Completed { evidence, .. } => evidence,
                    outcome => {
                        return Err(recovery_error(
                            ErrorCode::VerificationFailed,
                            format!("durable restart navigation failed: {outcome:?}"),
                            false,
                        ));
                    }
                };
                if let RecoveryDecision::Restarted {
                    evidence: restart_evidence,
                    ..
                } = &mut restarted
                {
                    *restart_evidence = evidence;
                }
                self.recovery
                    .record_locked_decision(verified, &restarted)
                    .await
                    .map_err(recovery_coordinator_error)?;
                Ok(recovery_progress(
                    restarted,
                    SkillTacticEffect::DurableBoundaryRestarted,
                    envelope,
                ))
            }
        }
    }

    async fn after_replacement(
        &self,
        recovered: RecoveryDecision,
        effect: SkillTacticEffect,
        envelope: &CommandEnvelope,
        page: &PageState,
    ) -> Result<TacticProgress, CommandError> {
        self.sync_recovered_page(&recovered, envelope, page).await?;
        if matches!(recovered, RecoveryDecision::Resumed { .. }) {
            let observed = self.observe_postcondition(envelope, page).await?;
            if observed.0 {
                return Ok(TacticProgress::Completed(observed.1, effect));
            }
        }
        Ok(recovery_progress(recovered, effect, envelope))
    }

    async fn sync_recovered_page(
        &self,
        recovered: &RecoveryDecision,
        envelope: &CommandEnvelope,
        page: &PageState,
    ) -> Result<(), CommandError> {
        let observed_url = match recovered {
            RecoveryDecision::Resumed { evidence, .. }
            | RecoveryDecision::NeedsReconciliation { evidence, .. } => {
                evidence.iter().rev().find_map(|item| match item {
                    Evidence::Navigation { url, .. } | Evidence::Inspection { url, .. } => {
                        Some(url.clone())
                    }
                    _ => None,
                })
            }
            RecoveryDecision::Restarted { .. } => Some(
                self.recovery
                    .load_checkpoint(&envelope.workflow_id)
                    .await
                    .map_err(recovery_coordinator_error)?
                    .restart_url,
            ),
        };
        if let Some(url) = observed_url {
            self.runtime
                .set_url(&page.id, url, "interactive")
                .await
                .map_err(|error| {
                    recovery_error(
                        ErrorCode::Internal,
                        format!("recovered page state update failed: {error}"),
                        false,
                    )
                })?;
        }
        Ok(())
    }

    async fn observe_postcondition(
        &self,
        envelope: &CommandEnvelope,
        page: &PageState,
    ) -> Result<(bool, Vec<Evidence>), CommandError> {
        let lease = self.workers.lease(envelope.session_id.clone()).await?;
        let inspect = match &envelope.command {
            RuntimeCommand::Primitive(PrimitiveCommand::TypeText(command)) => InspectCommand {
                selector: (!command.selector.is_empty()).then(|| command.selector.clone()),
                target: command.target.clone(),
                include_html: false,
            },
            RuntimeCommand::Primitive(PrimitiveCommand::Click(command)) => InspectCommand {
                selector: None,
                target: command.target.clone(),
                include_html: false,
            },
            _ => InspectCommand::default(),
        };
        let evidence = lease.worker().inspect(&page.id, &inspect).await?;
        let matched = match &envelope.command {
            RuntimeCommand::Primitive(PrimitiveCommand::Navigate(command)) => evidence.iter().any(
                |item| matches!(item, Evidence::Inspection { url, .. } if url == &command.url),
            ),
            RuntimeCommand::Primitive(PrimitiveCommand::Click(command)) => command.expected_url.as_ref().is_some_and(|url| {
                evidence.iter().any(
                    |item| matches!(item, Evidence::Inspection { url: actual, .. } if actual == url),
                )
            }),
            RuntimeCommand::Primitive(PrimitiveCommand::TypeText(command)) => evidence.iter().any(|item| {
                matches!(item, Evidence::Inspection { text, .. } if text == &command.value)
            }),
            RuntimeCommand::Primitive(PrimitiveCommand::Inspect(_)) => !evidence.is_empty(),
            _ => false,
        };
        Ok((matched, evidence))
    }
}
