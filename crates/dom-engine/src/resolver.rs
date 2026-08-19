use regex::Regex;
use thiserror::Error;
use types::{CandidateEvidence, TargetSpec, TextMatch};

use crate::Candidate;

#[derive(Debug, Clone)]
pub struct ResolutionPolicy {
    pub confidence_floor: i32,
    pub uniqueness_margin: i32,
    pub max_candidates: usize,
    pub max_regex_len: usize,
    pub require_visible: bool,
}

impl Default for ResolutionPolicy {
    fn default() -> Self {
        Self {
            confidence_floor: 1,
            uniqueness_margin: 1,
            max_candidates: 100,
            max_regex_len: 256,
            require_visible: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResolutionDecision {
    Resolved {
        candidate: Box<Candidate>,
        evidence: CandidateEvidence,
        best_match_authorized: bool,
    },
    Ambiguous {
        candidates: Vec<CandidateEvidence>,
    },
    NotFound,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ResolutionError {
    #[error("target regular expression exceeds configured limit")]
    RegexTooLong,
    #[error("invalid target regular expression: {0}")]
    InvalidRegex(String),
    #[error("candidate limit exceeded: {count} candidates (limit {limit}); first matches: {matches}. Narrow the target with role + accessibleName, label, testId, CSS, or ordinal")]
    CandidateLimitExceeded {
        count: usize,
        limit: usize,
        matches: String,
    },
}

pub fn resolve_candidates(
    target: &TargetSpec,
    candidates: &[Candidate],
    policy: &ResolutionPolicy,
) -> Result<ResolutionDecision, ResolutionError> {
    let regex = match &target.text {
        Some(TextMatch::Regex(pattern)) => {
            if pattern.len() > policy.max_regex_len {
                return Err(ResolutionError::RegexTooLong);
            }
            Some(
                Regex::new(pattern)
                    .map_err(|error| ResolutionError::InvalidRegex(error.to_string()))?,
            )
        }
        _ => None,
    };
    let mut ranked = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| {
            candidate.state.attached && (!policy.require_visible || candidate.state.visible)
        })
        .filter_map(|(index, candidate)| {
            score(target, candidate, regex.as_ref())
                .map(|(candidate, evidence)| (index, candidate, evidence))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .2
            .score
            .cmp(&left.2.score)
            .then_with(|| left.0.cmp(&right.0))
    });
    // An explicit ordinal picks one match deterministically, so a large
    // matching set is not ambiguity for it; the bound guards the ranked
    // choice below.
    if target.ordinal.is_none() && ranked.len() > policy.max_candidates {
        let matches = ranked
            .iter()
            .take(10)
            .map(|(_, candidate, _)| {
                format!(
                    "id={},role={},name={}",
                    bounded_candidate_field(&candidate.id),
                    bounded_candidate_field(candidate.role.as_deref().unwrap_or("-")),
                    bounded_candidate_field(candidate.name.as_deref().unwrap_or("-"))
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(ResolutionError::CandidateLimitExceeded {
            count: ranked.len(),
            limit: policy.max_candidates,
            matches,
        });
    }
    if ranked.is_empty() {
        return Ok(ResolutionDecision::NotFound);
    }
    if let Some(ordinal) = target.ordinal {
        return Ok(match ranked.get(ordinal) {
            Some((_, candidate, evidence)) => ResolutionDecision::Resolved {
                candidate: Box::new((*candidate).clone()),
                evidence: evidence.clone(),
                best_match_authorized: false,
            },
            None => ResolutionDecision::NotFound,
        });
    }
    let unique =
        ranked.len() == 1 || ranked[0].2.score - ranked[1].2.score >= policy.uniqueness_margin;
    let confident = ranked[0].2.score >= policy.confidence_floor;
    if confident && (unique || target.allow_best_match) {
        let (_, candidate, evidence) = &ranked[0];
        return Ok(ResolutionDecision::Resolved {
            candidate: Box::new((*candidate).clone()),
            evidence: evidence.clone(),
            best_match_authorized: !unique && target.allow_best_match,
        });
    }
    Ok(ResolutionDecision::Ambiguous {
        candidates: ranked
            .into_iter()
            .map(|(_, _, evidence)| evidence)
            .collect(),
    })
}

fn bounded_candidate_field(value: &str) -> String {
    const MAX_CHARS: usize = 80;
    let mut chars = value.chars();
    let bounded = chars.by_ref().take(MAX_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{bounded}...")
    } else {
        bounded
    }
}

/// Case-insensitive role match that also treats `img` and `image` as the
/// same role: Chrome's a11y tree moved from `img` to `image`, but the DOM
/// collector's implicit-role mapping still emits `img` for an `<img>`
/// element, so a snapshot role fed back as a target must resolve either way.
fn roles_match(wanted: &str, actual: &str) -> bool {
    if wanted.eq_ignore_ascii_case(actual) {
        return true;
    }
    let is_img_alias =
        |role: &str| role.eq_ignore_ascii_case("img") || role.eq_ignore_ascii_case("image");
    is_img_alias(wanted) && is_img_alias(actual)
}

fn score<'a>(
    target: &TargetSpec,
    candidate: &'a Candidate,
    regex: Option<&Regex>,
) -> Option<(&'a Candidate, CandidateEvidence)> {
    let mut score = 0;
    let mut reasons = Vec::new();
    macro_rules! exact {
        ($wanted:expr, $actual:expr, $points:expr, $reason:expr) => {
            if let Some(wanted) = $wanted {
                if $actual != Some(wanted) {
                    return None;
                }
                score += $points;
                reasons.push($reason.into());
            }
        };
    }
    exact!(target.css.as_ref(), candidate.css.as_ref(), 100, "exactCss");
    exact!(
        target.test_id.as_ref(),
        candidate.test_id.as_ref(),
        100,
        "exactTestId"
    );
    // Roles are ASCII tokens; the a11y snapshot emits the engine's casing
    // (Chrome's `Iframe`) while DOM candidates carry the lowercase implicit
    // role, so a snapshot target passed back verbatim must match either.
    if let Some(wanted) = target.role.as_ref() {
        if !candidate
            .role
            .as_ref()
            .is_some_and(|actual| roles_match(wanted, actual))
        {
            return None;
        }
        score += 30;
        reasons.push("exactRole".into());
    }
    if let Some(wanted) = target.accessible_name.as_ref() {
        if candidate.name.as_deref().map(str::trim) != Some(wanted.trim()) {
            return None;
        }
        score += 50;
        reasons.push("exactAccessibleName".into());
    }
    exact!(
        target.label.as_ref(),
        candidate.label.as_ref(),
        50,
        "exactLabel"
    );
    for (name, value) in &target.attributes {
        if candidate.attributes.get(name) != Some(value) {
            return None;
        }
        score += 20;
        reasons.push(format!("attribute:{name}"));
    }
    if let Some(matcher) = &target.text {
        let matched = match matcher {
            TextMatch::Exact(value) => candidate.text == *value,
            TextMatch::Contains(value) => candidate.text.contains(value),
            TextMatch::Regex(_) => regex.is_some_and(|regex| regex.is_match(&candidate.text)),
        };
        if !matched {
            return None;
        }
        score += match matcher {
            TextMatch::Exact(_) => 40,
            TextMatch::Contains(_) => 20,
            TextMatch::Regex(_) => 15,
        };
        reasons.push("text".into());
    }
    Some((
        candidate,
        CandidateEvidence {
            role: candidate.role.clone(),
            name: candidate.name.clone(),
            score,
            reasons,
        },
    ))
}
