use async_trait::async_trait;
use dom_engine::{resolve_candidates, Candidate, ResolutionDecision, ResolutionPolicy};
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandError, ErrorCode, ErrorLayer, Evidence,
    ExecutionRecord, FillValue, IntentCommand, PageId, TargetFingerprint, TargetSpec,
    TypeTextCommand, UploadFilesCommand, WaitForCommand,
};

use crate::compiler::{compile_intent, IntentPlan};
use crate::stuck::StuckKind;
use crate::verify::{compatible, execution_record, summarize_target, verify_fill};

#[derive(Debug, Clone, Copy, Default)]
pub struct VisionContext {
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub enum IntentOutcome {
    Completed {
        evidence: Vec<Evidence>,
    },
    Failed {
        error: CommandError,
        evidence: Vec<Evidence>,
    },
}

#[async_trait]
pub trait IntentBrowser: Send + Sync {
    async fn collect_candidates(
        &self,
        page_id: &PageId,
        target: &TargetSpec,
    ) -> Result<Vec<Candidate>, CommandError>;

    async fn click(
        &self,
        page_id: &PageId,
        command: &ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError>;

    async fn type_text(
        &self,
        page_id: &PageId,
        command: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError>;

    async fn upload_files(
        &self,
        page_id: &PageId,
        command: &UploadFilesCommand,
    ) -> Result<Vec<Evidence>, CommandError>;

    async fn wait_for(
        &self,
        page_id: &PageId,
        command: &WaitForCommand,
    ) -> Result<Vec<Evidence>, CommandError>;

    async fn capture_screenshot(
        &self,
        page_id: &PageId,
        command: &CaptureScreenshotCommand,
    ) -> Result<Vec<Evidence>, CommandError>;
}

pub struct IntentEngine;

impl IntentEngine {
    pub async fn execute(
        intent: &IntentCommand,
        page_id: &PageId,
        browser: &dyn IntentBrowser,
        // Task 4 keeps vision off; Task 7 wires escalation through this context.
        _vision: &VisionContext,
    ) -> IntentOutcome {
        let plan = match compile_intent(intent) {
            Ok(plan) => plan,
            Err(error) => {
                return IntentOutcome::Failed {
                    error: CommandError {
                        code: ErrorCode::IntentCompileFailed,
                        message: error.to_string(),
                        layer: ErrorLayer::Page,
                        retryable: false,
                    },
                    evidence: Vec::new(),
                };
            }
        };

        match plan {
            IntentPlan::Locate { target } => {
                execute_locate(intent, page_id, browser, target).await
            }
            IntentPlan::WaitForState {
                condition,
                timeout_ms,
            } => {
                execute_wait_for_state(page_id, browser, condition, timeout_ms).await
            }
            IntentPlan::Fill { target, value } => {
                execute_fill(intent, page_id, browser, target, value).await
            }
            IntentPlan::SubmitAndVerify { .. } => IntentOutcome::Failed {
                error: CommandError {
                    code: ErrorCode::Internal,
                    message: "not yet implemented".into(),
                    layer: ErrorLayer::Page,
                    retryable: false,
                },
                evidence: Vec::new(),
            },
        }
    }
}

async fn execute_locate(
    intent: &IntentCommand,
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    target: TargetSpec,
) -> IntentOutcome {
    let purpose = match intent {
        IntentCommand::Locate(locate) => Some(locate.purpose.clone()),
        _ => None,
    };
    let plan_summary = summarize_target(&target);
    let candidates = match browser.collect_candidates(page_id, &target).await {
        Ok(candidates) => candidates,
        Err(error) => {
            return IntentOutcome::Failed {
                error,
                evidence: vec![intent_evidence(execution_record(
                    "locate",
                    purpose,
                    plan_summary,
                    Vec::new(),
                    None,
                    "gatherFailed",
                ))],
            };
        }
    };

    let decision = match resolve_candidates(&target, &candidates, &ResolutionPolicy::default()) {
        Ok(decision) => decision,
        Err(error) => {
            return IntentOutcome::Failed {
                error: CommandError {
                    code: ErrorCode::InvalidRequest,
                    message: error.to_string(),
                    layer: ErrorLayer::Page,
                    retryable: false,
                },
                evidence: vec![intent_evidence(execution_record(
                    "locate",
                    purpose,
                    plan_summary,
                    Vec::new(),
                    None,
                    "resolveFailed",
                ))],
            };
        }
    };

    match decision {
        ResolutionDecision::Resolved {
            candidate,
            evidence,
            best_match_authorized,
        } => {
            let fingerprint = fingerprint(page_id, &candidate);
            let resolution = Evidence::Resolution {
                target: Box::new(target),
                fingerprint: Box::new(fingerprint),
                candidates: vec![evidence.clone()],
                best_match_authorized,
            };
            let record = execution_record(
                "locate",
                purpose,
                plan_summary,
                vec![evidence],
                None,
                "resolved",
            );
            IntentOutcome::Completed {
                evidence: vec![resolution, intent_evidence(record)],
            }
        }
        ResolutionDecision::NotFound => stuck_outcome(
            "locate",
            StuckKind::TargetMissing,
            purpose,
            plan_summary,
            Vec::new(),
            "targetNotFound",
        ),
        ResolutionDecision::Ambiguous { candidates } => stuck_outcome(
            "locate",
            StuckKind::TargetAmbiguous,
            purpose,
            plan_summary,
            candidates,
            "targetAmbiguous",
        ),
    }
}

async fn execute_fill(
    intent: &IntentCommand,
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    target: TargetSpec,
    value: FillValue,
) -> IntentOutcome {
    let purpose = match intent {
        IntentCommand::Fill(fill) => Some(fill.purpose.clone()),
        _ => None,
    };
    let plan_summary = format!("{} value={}", summarize_target(&target), fill_kind(&value));
    let candidates = match browser.collect_candidates(page_id, &target).await {
        Ok(candidates) => candidates,
        Err(error) => {
            return IntentOutcome::Failed {
                error,
                evidence: vec![intent_evidence(execution_record(
                    "fill",
                    purpose,
                    plan_summary,
                    Vec::new(),
                    None,
                    "gatherFailed",
                ))],
            };
        }
    };

    let decision = match resolve_candidates(&target, &candidates, &ResolutionPolicy::default()) {
        Ok(decision) => decision,
        Err(error) => {
            return IntentOutcome::Failed {
                error: CommandError {
                    code: ErrorCode::InvalidRequest,
                    message: error.to_string(),
                    layer: ErrorLayer::Page,
                    retryable: false,
                },
                evidence: vec![intent_evidence(execution_record(
                    "fill",
                    purpose,
                    plan_summary,
                    Vec::new(),
                    None,
                    "resolveFailed",
                ))],
            };
        }
    };

    let (candidate, candidate_evidence, best_match_authorized) = match decision {
        ResolutionDecision::Resolved {
            candidate,
            evidence,
            best_match_authorized,
        } => (candidate, evidence, best_match_authorized),
        ResolutionDecision::NotFound => {
            return stuck_outcome(
                "fill",
                StuckKind::TargetMissing,
                purpose,
                plan_summary,
                Vec::new(),
                "targetNotFound",
            );
        }
        ResolutionDecision::Ambiguous { candidates } => {
            return stuck_outcome(
                "fill",
                StuckKind::TargetAmbiguous,
                purpose,
                plan_summary,
                candidates,
                "targetAmbiguous",
            );
        }
    };

    if !compatible(&value, &candidate) {
        return IntentOutcome::Failed {
            error: CommandError {
                code: ErrorCode::IntentActionMismatch,
                message: format!(
                    "fill {} is incompatible with resolved control role={:?} type={:?}",
                    fill_kind(&value),
                    candidate.role,
                    candidate.attributes.get("type")
                ),
                layer: ErrorLayer::Page,
                retryable: false,
            },
            evidence: vec![intent_evidence(execution_record(
                "fill",
                purpose,
                plan_summary,
                vec![candidate_evidence],
                None,
                "actionMismatch",
            ))],
        };
    }

    let fingerprint = fingerprint(page_id, &candidate);
    let resolution = Evidence::Resolution {
        target: Box::new(target.clone()),
        fingerprint: Box::new(fingerprint),
        candidates: vec![candidate_evidence.clone()],
        best_match_authorized,
    };

    let mut act_evidence = match act_fill(page_id, browser, &candidate, &value).await {
        Ok(evidence) => evidence,
        Err(error) => {
            return IntentOutcome::Failed {
                error,
                evidence: vec![
                    resolution,
                    intent_evidence(execution_record(
                        "fill",
                        purpose,
                        plan_summary,
                        vec![candidate_evidence],
                        None,
                        "actFailed",
                    )),
                ],
            };
        }
    };

    if let Err(message) = verify_fill(&value, &act_evidence) {
        return IntentOutcome::Failed {
            error: CommandError {
                code: ErrorCode::VerificationFailed,
                message,
                layer: ErrorLayer::Page,
                retryable: false,
            },
            evidence: {
                let mut evidence = vec![resolution];
                evidence.append(&mut act_evidence);
                evidence.push(intent_evidence(execution_record(
                    "fill",
                    purpose,
                    plan_summary,
                    vec![candidate_evidence],
                    None,
                    "verifyFailed",
                )));
                evidence
            },
        };
    }

    let mut evidence = vec![resolution];
    evidence.append(&mut act_evidence);
    evidence.push(intent_evidence(execution_record(
        "fill",
        purpose,
        plan_summary,
        vec![candidate_evidence],
        None,
        "filled",
    )));
    IntentOutcome::Completed { evidence }
}

async fn act_fill(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    candidate: &Candidate,
    value: &FillValue,
) -> Result<Vec<Evidence>, CommandError> {
    let (selector, target) = action_target(candidate);
    match value {
        // Worker-pool has no select API; Select is typed via TypeTextCommand.
        FillValue::Text { text, clear_first } => {
            browser
                .type_text(
                    page_id,
                    &TypeTextCommand {
                        selector,
                        target: Some(target),
                        value: text.clone(),
                        clear_first: *clear_first,
                    },
                )
                .await
        }
        FillValue::Select { option } => {
            browser
                .type_text(
                    page_id,
                    &TypeTextCommand {
                        selector,
                        target: Some(target),
                        value: option.clone(),
                        clear_first: true,
                    },
                )
                .await
        }
        FillValue::Files { paths } => {
            browser
                .upload_files(
                    page_id,
                    &UploadFilesCommand {
                        selector,
                        target: Some(target),
                        paths: paths.clone(),
                    },
                )
                .await
        }
    }
}

fn action_target(candidate: &Candidate) -> (String, TargetSpec) {
    let selector = candidate.css.clone().unwrap_or_default();
    let target = TargetSpec {
        css: candidate.css.clone(),
        test_id: candidate.test_id.clone(),
        role: candidate.role.clone(),
        accessible_name: candidate.name.clone(),
        label: candidate.label.clone(),
        attributes: candidate.attributes.clone(),
        ..TargetSpec::default()
    };
    (selector, target)
}

fn fingerprint(page_id: &PageId, candidate: &Candidate) -> TargetFingerprint {
    TargetFingerprint {
        page_id: page_id.clone(),
        frame: None,
        role: candidate.role.clone(),
        name: candidate.name.clone(),
        stable_attributes: candidate.attributes.clone(),
    }
}

fn fill_kind(value: &FillValue) -> &'static str {
    match value {
        FillValue::Text { .. } => "text",
        FillValue::Select { .. } => "select",
        FillValue::Files { .. } => "files",
    }
}

async fn execute_wait_for_state(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    condition: types::WaitCondition,
    timeout_ms: u64,
) -> IntentOutcome {
    let command = WaitForCommand {
        condition: condition.clone(),
        timeout_ms,
    };
    let plan_summary = format!("wait timeout_ms={timeout_ms}");
    match browser.wait_for(page_id, &command).await {
        Ok(mut evidence) => {
            let wait_elapsed_ms = evidence.iter().find_map(|item| match item {
                Evidence::Wait { elapsed_ms, .. } => Some(*elapsed_ms),
                _ => None,
            });
            evidence.push(intent_evidence(execution_record(
                "waitForState",
                None,
                plan_summary,
                Vec::new(),
                wait_elapsed_ms,
                "waitSatisfied",
            )));
            IntentOutcome::Completed { evidence }
        }
        Err(error) => IntentOutcome::Failed {
            error,
            evidence: vec![intent_evidence(execution_record(
                "waitForState",
                None,
                plan_summary,
                Vec::new(),
                None,
                "waitFailed",
            ))],
        },
    }
}

fn stuck_outcome(
    intent_kind: &str,
    kind: StuckKind,
    purpose: Option<String>,
    plan_summary: String,
    candidates: Vec<types::CandidateEvidence>,
    verification: &str,
) -> IntentOutcome {
    IntentOutcome::Failed {
        error: CommandError {
            code: kind.error_code(),
            message: verification.to_owned(),
            layer: ErrorLayer::Page,
            retryable: false,
        },
        evidence: vec![intent_evidence(execution_record(
            intent_kind,
            purpose,
            plan_summary,
            candidates,
            None,
            verification,
        ))],
    }
}

fn intent_evidence(record: ExecutionRecord) -> Evidence {
    Evidence::IntentExecution { record }
}
