use std::collections::HashSet;

use thiserror::Error;
use types::{
    ExtractValueKind, FillValue, IntentCommand, IntentHints, TargetSpec, TextMatch, WaitCondition,
    WaitForCommand, MAX_INTENT_PURPOSE_BYTES,
};

const MAX_FORM_FIELDS: usize = 128;

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

#[derive(Debug, Clone)]
pub struct CompleteFormFieldPlan {
    pub name: String,
    pub purpose: String,
    pub target: TargetSpec,
    pub value: FillValue,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CompileError {
    #[error("intent purpose must not be empty")]
    EmptyPurpose,
    #[error("intent purpose exceeds {MAX_INTENT_PURPOSE_BYTES} bytes")]
    PurposeTooLong,
    #[error("extract intent must include at least one field")]
    NoExtractFields,
    #[error("complete form intent must include at least one field")]
    NoFormFields,
    #[error("complete form intent exceeds {MAX_FORM_FIELDS} fields")]
    TooManyFormFields,
    #[error("extract field name must not be empty")]
    EmptyFieldName,
    #[error("duplicate extract field name: {0}")]
    DuplicateFieldName(String),
    #[error("hints set accessibleName and an exact nearText to different values")]
    ConflictingNameHints,
}

pub fn compile_intent(command: &IntentCommand) -> Result<IntentPlan, CompileError> {
    match command {
        IntentCommand::Locate(intent) => {
            let purpose = validate_purpose(&intent.purpose)?;
            Ok(IntentPlan::Locate {
                target: compile_locate_target(purpose, &intent.hints)?,
            })
        }
        IntentCommand::Fill(intent) => {
            let purpose = validate_purpose(&intent.purpose)?;
            Ok(IntentPlan::Fill {
                target: compile_target(purpose, &intent.hints)?,
                value: intent.value.clone(),
            })
        }
        IntentCommand::CompleteForm(intent) => {
            validate_purpose(&intent.purpose)?;
            if intent.fields.is_empty() {
                return Err(CompileError::NoFormFields);
            }
            if intent.fields.len() > MAX_FORM_FIELDS {
                return Err(CompileError::TooManyFormFields);
            }
            let mut names = HashSet::new();
            let mut fields = Vec::with_capacity(intent.fields.len());
            for field in &intent.fields {
                if field.name.trim().is_empty() {
                    return Err(CompileError::EmptyFieldName);
                }
                if !names.insert(field.name.clone()) {
                    return Err(CompileError::DuplicateFieldName(field.name.clone()));
                }
                let purpose = validate_purpose(&field.purpose)?;
                fields.push(CompleteFormFieldPlan {
                    name: field.name.clone(),
                    purpose: purpose.into(),
                    target: compile_target(purpose, &field.hints)?,
                    value: field.value.clone(),
                });
            }
            Ok(IntentPlan::CompleteForm { fields })
        }
        IntentCommand::SubmitAndVerify(intent) => {
            let purpose = validate_purpose(&intent.purpose)?;
            Ok(IntentPlan::SubmitAndVerify {
                target: compile_target(purpose, &intent.hints)?,
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
                target: compile_target(purpose, &intent.hints)?,
                expected_destination: intent.expected_destination.clone(),
                boundary: intent.boundary,
            })
        }
        IntentCommand::DismissObstruction(intent) => {
            let purpose = validate_purpose(&intent.purpose)?;
            Ok(IntentPlan::DismissObstruction {
                target: compile_target(purpose, &intent.hints)?,
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
                    target: compile_target(purpose, &field.hints)?,
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

fn compile_target(purpose: &str, hints: &IntentHints) -> Result<TargetSpec, CompileError> {
    compile_target_with_purpose_fallback(purpose, hints, true)
}

fn compile_locate_target(purpose: &str, hints: &IntentHints) -> Result<TargetSpec, CompileError> {
    compile_target_with_purpose_fallback(purpose, hints, false)
}

fn compile_target_with_purpose_fallback(
    purpose: &str,
    hints: &IntentHints,
    purpose_names_role_target: bool,
) -> Result<TargetSpec, CompileError> {
    let mut target = TargetSpec {
        role: hints.role.clone(),
        ordinal: hints.ordinal,
        frame_path: hints.frame_path.iter().cloned().map(Box::new).collect(),
        shadow_path: hints.shadow_path.iter().cloned().map(Box::new).collect(),
        allow_best_match: hints.allow_best_match,
        ..TargetSpec::default()
    };

    // `accessibleName` is the snapshot-shaped spelling of an exact `nearText`.
    // Two different names is a caller mistake with no safe reading, so refuse
    // instead of silently preferring one.
    let name_hint = match (&hints.accessible_name, &hints.near_text) {
        (Some(name), Some(TextMatch::Exact(near))) if name != near => {
            return Err(CompileError::ConflictingNameHints);
        }
        (Some(name), _) => Some(TextMatch::Exact(name.clone())),
        (None, matcher) => matcher.clone(),
    };

    match &name_hint {
        Some(TextMatch::Exact(name)) if hints.role.is_some() => {
            target.accessible_name = Some(name.clone());
        }
        Some(matcher) => {
            target.text = Some(matcher.clone());
        }
        // Locate purposes may be descriptive prose and use semantic token
        // overlap later. Other intents historically use purpose as the exact
        // control name; dropping that fallback makes ordinary forms ambiguous.
        None if hints.role.is_some() && purpose_names_role_target => {
            target.accessible_name = Some(purpose.to_owned());
        }
        None if hints.role.is_some() => {}
        None => {
            target.text = Some(TextMatch::Contains(purpose.to_owned()));
        }
    }

    Ok(target)
}
