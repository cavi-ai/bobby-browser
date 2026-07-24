use thiserror::Error;
use types::{
    FillValue, IntentCommand, IntentHints, TargetSpec, TextMatch, WaitCondition, WaitForCommand,
    MAX_INTENT_PURPOSE_BYTES,
};

#[derive(Debug, Clone)]
pub enum IntentPlan {
    Locate {
        target: TargetSpec,
    },
    Fill {
        target: TargetSpec,
        value: FillValue,
    },
    SubmitAndVerify {
        target: TargetSpec,
        expected_state: WaitForCommand,
    },
    WaitForState {
        condition: WaitCondition,
        timeout_ms: u64,
    },
    Follow {
        target: TargetSpec,
        expected_destination: WaitForCommand,
        boundary: bool,
    },
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum CompileError {
    #[error("intent purpose must not be empty")]
    EmptyPurpose,
    #[error("intent purpose exceeds {MAX_INTENT_PURPOSE_BYTES} bytes")]
    PurposeTooLong,
}

pub fn compile_intent(command: &IntentCommand) -> Result<IntentPlan, CompileError> {
    match command {
        IntentCommand::Locate(intent) => {
            let purpose = validate_purpose(&intent.purpose)?;
            Ok(IntentPlan::Locate {
                target: compile_target(purpose, &intent.hints),
            })
        }
        IntentCommand::Fill(intent) => {
            let purpose = validate_purpose(&intent.purpose)?;
            Ok(IntentPlan::Fill {
                target: compile_target(purpose, &intent.hints),
                value: intent.value.clone(),
            })
        }
        IntentCommand::SubmitAndVerify(intent) => {
            let purpose = validate_purpose(&intent.purpose)?;
            Ok(IntentPlan::SubmitAndVerify {
                target: compile_target(purpose, &intent.hints),
                expected_state: intent.expected_state.clone(),
            })
        }
        IntentCommand::WaitForState(intent) => Ok(IntentPlan::WaitForState {
            condition: intent.condition.clone(),
            timeout_ms: intent.timeout_ms,
        }),
        IntentCommand::Follow(intent) => {
            let purpose = validate_purpose(&intent.purpose)?;
            Ok(IntentPlan::Follow {
                target: compile_target(purpose, &intent.hints),
                expected_destination: intent.expected_destination.clone(),
                boundary: intent.boundary,
            })
        }
    }
}

fn validate_purpose(purpose: &str) -> Result<&str, CompileError> {
    if purpose.len() > MAX_INTENT_PURPOSE_BYTES {
        return Err(CompileError::PurposeTooLong);
    }
    let trimmed = purpose.trim();
    if trimmed.is_empty() {
        return Err(CompileError::EmptyPurpose);
    }
    Ok(trimmed)
}

fn compile_target(purpose: &str, hints: &IntentHints) -> TargetSpec {
    let mut target = TargetSpec {
        role: hints.role.clone(),
        frame_path: hints
            .frame_path
            .iter()
            .cloned()
            .map(Box::new)
            .collect(),
        shadow_path: hints
            .shadow_path
            .iter()
            .cloned()
            .map(Box::new)
            .collect(),
        allow_best_match: hints.allow_best_match,
        ..TargetSpec::default()
    };

    if hints.role.is_some() {
        target.accessible_name = Some(purpose.to_owned());
    } else {
        target.text = Some(TextMatch::Contains(purpose.to_owned()));
    }

    target
}
