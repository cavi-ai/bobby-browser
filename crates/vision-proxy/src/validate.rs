use crate::wire::{ExtractResponse, ProposeResponse, VisionAction};

const MAX_TEXT_BYTES: usize = 4096;
/// Serialized-size bound on an upstream extract value: the proxy forwards
/// this JSON to the runtime, so an unbounded upstream response would become
/// an unbounded downstream one.
const MAX_EXTRACT_VALUE_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ValidateError {
    #[error("vision proposal confidence is out of range")]
    ConfidenceOutOfRange,
    #[error("vision click coordinates are not finite")]
    NonFiniteClick,
    #[error("vision type text exceeded its bound")]
    TypeTextTooLong,
    #[error("vision extract value exceeded its bound")]
    ExtractValueTooLong,
    #[error("candidate action is incompatible with the request intent")]
    CandidateIntentMismatch,
    #[error("challengeSolved is only valid for a solveChallenge request")]
    ChallengeIntentMismatch,
    #[error("detection actions are only valid for a detectChallenge request")]
    DetectIntentMismatch,
    #[error("challenge region coordinates are not finite")]
    NonFiniteRegion,
    #[error("candidate action index is outside the request candidate list")]
    CandidateIndexOutOfRange,
}

pub fn validate_proposal(proposal: &ProposeResponse) -> Result<(), ValidateError> {
    if !proposal.confidence.is_finite() || !(0.0..=1.0).contains(&proposal.confidence) {
        return Err(ValidateError::ConfidenceOutOfRange);
    }

    match &proposal.action {
        VisionAction::Click { x, y } if x.is_finite() && y.is_finite() => Ok(()),
        VisionAction::Click { .. } => Err(ValidateError::NonFiniteClick),
        VisionAction::TypeText { text } if text.len() <= MAX_TEXT_BYTES => Ok(()),
        VisionAction::TypeText { .. } => Err(ValidateError::TypeTextTooLong),
        VisionAction::ExtractValue { value } if value.len() <= MAX_TEXT_BYTES => Ok(()),
        VisionAction::ExtractValue { .. } => Err(ValidateError::ExtractValueTooLong),
        VisionAction::ClickCandidate { .. } => Ok(()),
        VisionAction::TypeIntoCandidate { .. } | VisionAction::ExtractFromCandidate { .. } => {
            Ok(())
        }
        VisionAction::ChallengeSolved => Ok(()),
        VisionAction::ChallengeDetected { region, .. } => match region {
            Some(region)
                if !(region.x.is_finite()
                    && region.y.is_finite()
                    && region.width.is_finite()
                    && region.height.is_finite()) =>
            {
                Err(ValidateError::NonFiniteRegion)
            }
            _ => Ok(()),
        },
        VisionAction::NoChallengeDetected => Ok(()),
    }
}

pub fn validate_proposal_for_request(
    proposal: &ProposeResponse,
    intent_kind: &str,
    candidate_count: usize,
) -> Result<(), ValidateError> {
    validate_proposal(proposal)?;
    // challengeSolved is only meaningful as the terminal answer to a solve
    // request; any other intent receiving it is an upstream confusion.
    if matches!(proposal.action, VisionAction::ChallengeSolved) && intent_kind != "solveChallenge" {
        return Err(ValidateError::ChallengeIntentMismatch);
    }
    // Detection answers are the whole point of a detect request and noise
    // anywhere else; a detect request receiving anything else is equally
    // confused.
    let is_detect_action = matches!(
        proposal.action,
        VisionAction::ChallengeDetected { .. } | VisionAction::NoChallengeDetected
    );
    if is_detect_action != (intent_kind == "detectChallenge") {
        return Err(ValidateError::DetectIntentMismatch);
    }
    let (index, compatible) = match proposal.action {
        VisionAction::ClickCandidate { index } => (
            Some(index),
            matches!(
                intent_kind,
                "locate" | "submitAndVerify" | "follow" | "dismissObstruction"
            ),
        ),
        VisionAction::TypeIntoCandidate { index } => {
            (Some(index), matches!(intent_kind, "fill" | "type"))
        }
        VisionAction::ExtractFromCandidate { index } => (Some(index), intent_kind == "extract"),
        _ => (None, true),
    };
    if !compatible {
        return Err(ValidateError::CandidateIntentMismatch);
    }
    if let Some(index) = index {
        let index = usize::try_from(index).map_err(|_| ValidateError::CandidateIndexOutOfRange)?;
        if candidate_count == 0 || index >= candidate_count {
            return Err(ValidateError::CandidateIndexOutOfRange);
        }
    }
    Ok(())
}

pub fn validate_extract(response: &ExtractResponse) -> Result<(), ValidateError> {
    let serialized =
        serde_json::to_string(&response.value).map_err(|_| ValidateError::ExtractValueTooLong)?;
    if serialized.len() > MAX_EXTRACT_VALUE_BYTES {
        return Err(ValidateError::ExtractValueTooLong);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{ProposeResponse, VisionAction};

    #[test]
    fn rejects_confidence_outside_unit_interval() {
        let bad = ProposeResponse {
            confidence: 1.5,
            action: VisionAction::Click { x: 1.0, y: 2.0 },
        };
        assert!(validate_proposal(&bad).is_err());
    }

    #[test]
    fn rejects_non_finite_click() {
        let bad = ProposeResponse {
            confidence: 0.9,
            action: VisionAction::Click {
                x: f64::NAN,
                y: 1.0,
            },
        };
        assert!(validate_proposal(&bad).is_err());
    }

    #[test]
    fn accepts_in_range_click() {
        let ok = ProposeResponse {
            confidence: 0.9,
            action: VisionAction::Click { x: 12.0, y: 34.0 },
        };
        assert!(validate_proposal(&ok).is_ok());
    }

    #[test]
    fn candidate_actions_round_trip_with_camel_case_provider_tags() {
        for json in [
            r#"{"confidence":0.9,"action":{"kind":"typeIntoCandidate","index":1}}"#,
            r#"{"confidence":0.9,"action":{"kind":"extractFromCandidate","index":1}}"#,
        ] {
            let proposal: ProposeResponse = serde_json::from_str(json).unwrap();
            assert_eq!(serde_json::to_string(&proposal).unwrap(), json);
            assert!(validate_proposal(&proposal).is_ok());
        }
    }

    #[test]
    fn extract_rejects_an_oversized_value() {
        let too_big = ExtractResponse {
            value: serde_json::json!({"data": "x".repeat(128 * 1024)}),
        };
        assert_eq!(
            validate_extract(&too_big),
            Err(ValidateError::ExtractValueTooLong)
        );
    }

    #[test]
    fn extract_accepts_an_in_range_value() {
        let ok = ExtractResponse {
            value: serde_json::json!({"title": "Example", "price": 42}),
        };
        assert!(validate_extract(&ok).is_ok());
    }
}
