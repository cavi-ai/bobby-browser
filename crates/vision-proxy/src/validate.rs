use crate::wire::{ExtractResponse, ProposeResponse, VisionAction};

const MAX_TEXT_BYTES: usize = 4096;

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
    }
}

pub fn validate_extract(_response: &ExtractResponse) -> Result<(), ValidateError> {
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
}
