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
    } else if target.role.as_deref() == Some("button")
        && target.accessible_name.is_some()
        && target.attributes.len() == 1
        && target.attributes.get("type").map(String::as_str) == Some("file")
        && target.label.is_none()
        && target.text.is_none()
        && !target.allow_best_match
        && target.frame_path.is_empty()
    {
        json!({"role":"button","name":target.accessible_name,"inputType":"file"})
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
        ResolutionDecision::NotFound => host_by_snapshot_name(target, candidates),
        ResolutionDecision::Ambiguous { .. } => Err(shadow_probe_error("host-ambiguous").unwrap()),
    }
}

/// A form snapshot names a host without a role and label by `group` and its
/// `aria-label`, else its id, else `shadow root`; that name resolves it too.
fn host_by_snapshot_name(
    target: &TargetSpec,
    candidates: &[Candidate],
) -> Result<(Candidate, CandidateEvidence, bool), CommandError> {
    let missing = || shadow_probe_error("host-missing").unwrap();
    let (Some(role), Some(name)) = (target.role.as_deref(), target.accessible_name.as_deref())
    else {
        return Err(missing());
    };
    let snapshot_name = |candidate: &Candidate| {
        ["aria-label", "id"]
            .into_iter()
            .find_map(|key| {
                candidate
                    .attributes
                    .get(key)
                    .map(|value| value.split_whitespace().collect::<Vec<_>>().join(" "))
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or_else(|| "shadow root".to_owned())
    };
    let matches = candidates
        .iter()
        .filter(|candidate| candidate.state.attached && candidate.state.visible)
        .filter(|candidate| candidate.role.as_deref().unwrap_or("group") == role)
        .filter(|candidate| snapshot_name(candidate) == name.trim())
        .collect::<Vec<_>>();
    let candidate = match (target.ordinal, matches.as_slice()) {
        (Some(ordinal), _) => matches.get(ordinal).copied().ok_or_else(missing)?,
        (None, [only]) => *only,
        (None, []) => return Err(missing()),
        (None, _) => return Err(shadow_probe_error("host-ambiguous").unwrap()),
    };
    let evidence = CandidateEvidence {
        role: candidate.role.clone(),
        name: candidate.name.clone(),
        score: 0,
        reasons: vec!["shadowHostSnapshotName".into()],
    };
    Ok((candidate.clone(), evidence, false))
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
