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
    }
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
