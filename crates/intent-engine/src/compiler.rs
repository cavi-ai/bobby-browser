use std::collections::HashSet;

use thiserror::Error;
use types::{
    ExtractValueKind, FillValue, IntentCommand, IntentHints, TargetSpec, TextMatch, WaitCondition,
    WaitForCommand, MAX_INTENT_PURPOSE_BYTES,
};

const MAX_COMPLETE_FORM_FIELDS: usize = 128;

#[derive(Debug, Clone)]
pub enum IntentPlan {
    Locate {
        target: TargetSpec,
    },
    Fill {
        target: TargetSpec,
        value: FillValue,
    },
    CompleteForm {
        fields: Vec<CompleteFormFieldPlan>,
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
    DismissObstruction {
        target: TargetSpec,
        timeout_ms: u64,
    },
    Extract {
        fields: Vec<ExtractFieldPlan>,
    },
}

#[derive(Debug, Clone)]
pub struct CompleteFormFieldPlan {
    pub name: String,
    pub purpose: String,
    pub target: TargetSpec,
    pub value: FillValue,
}

#[derive(Debug, Clone)]
pub struct ExtractFieldPlan {
    pub name: String,
    /// Retained alongside `target` (which folds this into an accessible-name
    /// or text-contains match) because a per-field vision escalation needs
    /// the human-readable description of what to look for, not the compiled
    /// `TargetSpec`.
    pub purpose: String,
    pub target: TargetSpec,
    pub value: ExtractValueKind,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CompileError {
    #[error("intent purpose must not be empty")]
    EmptyPurpose,
    #[error("intent purpose exceeds {MAX_INTENT_PURPOSE_BYTES} bytes")]
    PurposeTooLong,
    #[error("extract intent must include at least one field")]
    NoExtractFields,
    #[error("extract field name must not be empty")]
    EmptyFieldName,
    #[error("duplicate extract field name: {0}")]
    DuplicateFieldName(String),
    #[error("complete form intent must include at least one field")]
    NoCompleteFormFields,
    #[error("complete form intent exceeds {MAX_COMPLETE_FORM_FIELDS} fields")]
    TooManyCompleteFormFields,
    #[error("complete form field name must not be empty")]
    EmptyCompleteFormFieldName,
    #[error("duplicate complete form field name: {0}")]
    DuplicateCompleteFormFieldName(String),
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
        IntentCommand::CompleteForm(intent) => {
            if intent.fields.is_empty() {
                return Err(CompileError::NoCompleteFormFields);
            }
            if intent.fields.len() > MAX_COMPLETE_FORM_FIELDS {
                return Err(CompileError::TooManyCompleteFormFields);
            }
            let mut names = HashSet::new();
            let mut fields = Vec::with_capacity(intent.fields.len());
            for field in &intent.fields {
                let name = field.name.trim();
                if name.is_empty() {
                    return Err(CompileError::EmptyCompleteFormFieldName);
                }
                if !names.insert(name.to_owned()) {
                    return Err(CompileError::DuplicateCompleteFormFieldName(
                        name.to_owned(),
                    ));
                }
                let purpose = validate_purpose(&field.purpose)?;
                fields.push(CompleteFormFieldPlan {
                    name: name.to_owned(),
                    purpose: purpose.to_owned(),
                    target: compile_target(purpose, &field.hints),
                    value: field.value.clone(),
                });
            }
            Ok(IntentPlan::CompleteForm { fields })
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
        IntentCommand::DismissObstruction(intent) => {
            let purpose = validate_purpose(&intent.purpose)?;
            Ok(IntentPlan::DismissObstruction {
                target: compile_target(purpose, &intent.hints),
                timeout_ms: intent.timeout_ms,
            })
        }
        IntentCommand::Extract(intent) => {
            validate_purpose(&intent.purpose)?;
            if intent.fields.is_empty() {
                return Err(CompileError::NoExtractFields);
            }
            let mut seen_names = HashSet::with_capacity(intent.fields.len());
            let mut fields = Vec::with_capacity(intent.fields.len());
            for field in &intent.fields {
                let name = field.name.trim();
                if name.is_empty() {
                    return Err(CompileError::EmptyFieldName);
                }
                if !seen_names.insert(name) {
                    return Err(CompileError::DuplicateFieldName(name.to_owned()));
                }
                let purpose = validate_purpose(&field.purpose)?;
                fields.push(ExtractFieldPlan {
                    name: name.to_owned(),
                    purpose: purpose.to_owned(),
                    target: compile_target(purpose, &field.hints),
                    value: field.value.clone(),
                });
            }
            Ok(IntentPlan::Extract { fields })
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
        frame_path: hints.frame_path.iter().cloned().map(Box::new).collect(),
        shadow_path: hints.shadow_path.iter().cloned().map(Box::new).collect(),
        allow_best_match: hints.allow_best_match,
        ..TargetSpec::default()
    };

    match &hints.near_text {
        Some(TextMatch::Exact(name)) if hints.role.is_some() => {
            target.accessible_name = Some(name.clone());
        }
        Some(matcher) => {
            target.text = Some(matcher.clone());
        }
        None if hints.role.is_some() => {
            target.accessible_name = Some(purpose.to_owned());
        }
        None => {
            target.text = Some(TextMatch::Contains(purpose.to_owned()));
        }
    }

    target
}
