use types::ErrorCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StuckKind {
    TargetMissing,
    TargetAmbiguous,
    ObstructionSuspected,
    VerifyNoDomSignal,
    /// Not a stuck state: `solveChallenge` escalates to vision as its
    /// primary path, so the propose request carries this marker.
    ChallengePresent,
}

impl StuckKind {
    pub fn may_escalate_to_vision(self) -> bool {
        true
    }

    pub fn error_code(self) -> ErrorCode {
        match self {
            Self::TargetMissing => ErrorCode::TargetNotFound,
            Self::TargetAmbiguous => ErrorCode::TargetAmbiguous,
            Self::ObstructionSuspected => ErrorCode::ObstructionSuspected,
            Self::VerifyNoDomSignal => ErrorCode::VerificationFailed,
            Self::ChallengePresent => ErrorCode::VisionAssistFailed,
        }
    }
}

pub fn never_escalates(code: ErrorCode) -> bool {
    matches!(
        code,
        ErrorCode::PolicyDenied
            | ErrorCode::ResourceExhausted
            | ErrorCode::InvalidRequest
            | ErrorCode::NetworkPolicyDenied
            | ErrorCode::IntentCompileFailed
            | ErrorCode::IntentActionMismatch
            | ErrorCode::DeadlineExceeded
    )
}
