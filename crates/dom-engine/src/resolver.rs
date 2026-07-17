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
}

impl Default for ResolutionPolicy {
    fn default() -> Self {
        Self {
            confidence_floor: 1,
            uniqueness_margin: 1,
            max_candidates: 100,
            max_regex_len: 256,
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
    #[error("candidate limit exceeded")]
    CandidateLimitExceeded,
}

pub fn resolve_candidates(
    target: &TargetSpec,
    candidates: &[Candidate],
    policy: &ResolutionPolicy,
) -> Result<ResolutionDecision, ResolutionError> {
    if candidates.len() > policy.max_candidates {
        return Err(ResolutionError::CandidateLimitExceeded);
    }
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
        .filter(|candidate| candidate.state.attached && candidate.state.visible)
        .filter_map(|candidate| score(target, candidate, regex.as_ref()))
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .1
            .score
            .cmp(&left.1.score)
            .then_with(|| left.0.id.cmp(&right.0.id))
    });
    if ranked.is_empty() {
        return Ok(ResolutionDecision::NotFound);
    }
    if let Some(ordinal) = target.ordinal {
        return Ok(match ranked.get(ordinal) {
            Some((candidate, evidence)) => ResolutionDecision::Resolved {
                candidate: Box::new((*candidate).clone()),
                evidence: evidence.clone(),
                best_match_authorized: false,
            },
            None => ResolutionDecision::NotFound,
        });
    }
    let unique =
        ranked.len() == 1 || ranked[0].1.score - ranked[1].1.score >= policy.uniqueness_margin;
    let confident = ranked[0].1.score >= policy.confidence_floor;
    if confident && (unique || target.allow_best_match) {
        let (candidate, evidence) = &ranked[0];
        return Ok(ResolutionDecision::Resolved {
            candidate: Box::new((*candidate).clone()),
            evidence: evidence.clone(),
            best_match_authorized: !unique && target.allow_best_match,
        });
    }
    Ok(ResolutionDecision::Ambiguous {
        candidates: ranked.into_iter().map(|(_, evidence)| evidence).collect(),
    })
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
    exact!(
        target.role.as_ref(),
        candidate.role.as_ref(),
        30,
        "exactRole"
    );
    exact!(
        target.accessible_name.as_ref(),
        candidate.name.as_ref(),
        50,
        "exactAccessibleName"
    );
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
