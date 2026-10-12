use crate::{resolve_candidates, Candidate, ResolutionDecision, ResolutionPolicy};
use serde_json::{json, Value};
use types::{CandidateEvidence, CommandError, ErrorCode, ErrorLayer, TargetSpec};

pub fn exact_target_selector(target: &TargetSpec) -> Option<String> {
    target
        .css
        .clone()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            target
                .test_id
                .as_ref()
                .filter(|value| !value.is_empty())
                .map(|id| {
                    let escaped = id.chars().fold(String::new(), |mut result, character| {
                        if matches!(character, '\\' | '"') {
                            result.push('\\');
                        }
                        result.push(character);
                        result
                    });
                    format!("[data-testid=\"{escaped}\"]")
                })
        })
}

/// Exact DOM identity for an adapter that evaluates the shared segment matcher.
pub fn shadow_segment_descriptor(target: &TargetSpec) -> Result<Value, CommandError> {
    let mut descriptor = if let Some(selector) = exact_target_selector(target) {
        json!({"selector":selector})
    } else if let (Some(role), Some(name)) = (&target.role, &target.accessible_name) {
        if !role.trim().is_empty()
            && !name.trim().is_empty()
            && target.attributes.is_empty()
            && target.label.is_none()
            && target.text.is_none()
            && !target.allow_best_match
            && target.frame_path.is_empty()
        {
            json!({"role":role,"name":name})
        } else {
            return Err(error(
                ErrorCode::InvalidRequest,
                "shadow segment requires exact CSS, test ID, or role and accessible name",
            ));
        }
    } else {
        return Err(error(
            ErrorCode::InvalidRequest,
            "shadow segment requires exact CSS, test ID, or role and accessible name",
        ));
    };
    if let Some(ordinal) = target.ordinal {
        descriptor["ordinal"] = json!(ordinal);
    }
    Ok(descriptor)
}

pub fn shadow_probe_error(verdict: &str) -> Option<CommandError> {
    match verdict {
        "host-missing" | "shadow-unavailable" => Some(error(
            ErrorCode::ShadowRootUnavailable,
            "shadow host or root is unavailable",
        )),
        "host-ambiguous" | "target-ambiguous" => Some(error(
            ErrorCode::TargetAmbiguous,
            "shadow path or target is ambiguous",
        )),
        "target-missing" => Some(error(
            ErrorCode::TargetNotFound,
            "shadow target was not found",
        )),
        "too-many" => Some(error(
            ErrorCode::BrowserCommandFailed,
            "shadow search exceeded its node bound",
        )),
        _ => None,
    }
}

/// Resolves one host with the same ranking as ordinary targets, and shared hop errors.
pub fn resolve_shadow_segment(
    target: &TargetSpec,
    candidates: &[Candidate],
) -> Result<(Candidate, CandidateEvidence, bool), CommandError> {
    match resolve_candidates(target, candidates, &ResolutionPolicy::default())
        .map_err(|error| self::error(ErrorCode::InvalidRequest, error.to_string()))?
    {
        ResolutionDecision::Resolved {
            candidate,
            evidence,
            best_match_authorized,
        } => Ok((*candidate, evidence, best_match_authorized)),
        ResolutionDecision::NotFound => Err(shadow_probe_error("host-missing").unwrap()),
        ResolutionDecision::Ambiguous { .. } => Err(shadow_probe_error("host-ambiguous").unwrap()),
    }
}

pub fn shadow_segment_function() -> &'static str {
    include_str!("shadow_segment.js")
}

fn error(code: ErrorCode, message: impl Into<String>) -> CommandError {
    CommandError {
        code,
        message: message.into(),
        layer: ErrorLayer::Driver,
        retryable: false,
    }
}
