//! What an ACP permission prompt is allowed to change.
//!
//! ACP's `session/request_permission` runs agent → client: bobby asks the
//! editor, the editor asks a human, the human clicks. That shape makes one
//! failure mode very easy to build by accident — treating a click as authority.
//! An editor user approving "allow this" cannot create authority the bearer
//! token never carried, and the runtime must not act as though it can.
//!
//! So the decision of whether to *ask at all* is made here, before any prompt
//! is sent, against the principal's capabilities:
//!
//! - The principal does not hold the capability → [`Escalation::Denied`].
//!   No prompt is sent. Asking would put a button in front of a human whose
//!   only honest outcome is failure, and a click that appears to work but
//!   does not is worse than a refusal.
//! - The principal holds it and session policy already permits it →
//!   [`Escalation::AlreadyPermitted`]. Nothing to ask.
//! - The principal holds it and session policy gates it →
//!   [`Escalation::AskUser`]. This is the only case that reaches a human, and
//!   the most a human can do is lift a gate over authority that already
//!   existed.
//!
//! This is the same shape as the vision double-gate: capability *and* session
//! policy, with the human able to move only the second one.

use types::{Capability, InterfaceOperation};

/// Whether an ACP permission prompt may be sent, and what it could change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Escalation {
    /// The principal already holds the capability and policy permits it.
    AlreadyPermitted,
    /// The principal holds the capability; session policy gates it. A human
    /// may lift that gate.
    AskUser { capability: Capability },
    /// The principal does not hold the capability. No prompt is sent, and no
    /// answer to one would change the outcome.
    Denied { missing: Capability },
}

/// Session policy flags an ACP prompt could lift, mirroring
/// `types::ExecutionPolicy`.
///
/// Only the gated-but-held flags appear here. `javascriptEvaluation`,
/// `fingerprint`, and `humanize` are deliberately absent: each is a
/// session-creation decision with its own capability, and a mid-session prompt
/// that flipped one would let a human widen a session past what its creator
/// asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionPolicyGates {
    pub vision_assist: bool,
}

/// What an ACP client is asking to do.
///
/// `requires_vision` is separate from `operation` because vision escalation is
/// not an interface operation — no `InterfaceOperation` names
/// `vision:assist`, since the double-gate lives in `intent-engine` at the point
/// a stuck intent would escalate. Folding it into the operation enum would
/// misplace the gate; passing it explicitly keeps both authorities visible at
/// the one place ACP could try to widen them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EscalationRequest {
    pub operation: InterfaceOperation,
    pub requires_vision: bool,
}

impl EscalationRequest {
    pub fn new(operation: InterfaceOperation) -> Self {
        Self {
            operation,
            requires_vision: false,
        }
    }

    pub fn with_vision(operation: InterfaceOperation) -> Self {
        Self {
            operation,
            requires_vision: true,
        }
    }
}

/// Decides whether `request` may be escalated for a principal holding
/// `capabilities` under `gates`.
pub fn decide(
    request: EscalationRequest,
    capabilities: &[Capability],
    gates: SessionPolicyGates,
) -> Escalation {
    let required = request.operation.required();
    // Missing capability wins over everything. Checked first and returned
    // immediately so no later branch can turn a missing capability into a
    // prompt.
    if let Some(missing) = required
        .iter()
        .find(|capability| !capabilities.contains(capability))
    {
        return Escalation::Denied { missing: *missing };
    }
    if request.requires_vision {
        if !capabilities.contains(&Capability::VisionAssist) {
            return Escalation::Denied {
                missing: Capability::VisionAssist,
            };
        }
        if !gates.vision_assist {
            return Escalation::AskUser {
                capability: Capability::VisionAssist,
            };
        }
    }
    Escalation::AlreadyPermitted
}

impl Escalation {
    /// Whether a prompt should be put in front of a human.
    pub fn should_prompt(&self) -> bool {
        matches!(self, Self::AskUser { .. })
    }

    /// Whether the operation may proceed once any prompt is answered
    /// affirmatively.
    ///
    /// `Denied` answers `false` regardless of what a human said, which is the
    /// property this whole module exists for.
    pub fn permits_after_approval(&self) -> bool {
        match self {
            Self::AlreadyPermitted | Self::AskUser { .. } => true,
            Self::Denied { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVERY_CAPABILITY: &[Capability] = &[
        Capability::SessionRead,
        Capability::SessionWrite,
        Capability::PageRead,
        Capability::PageWrite,
        Capability::BrowserMutate,
        Capability::FileUpload,
        Capability::FileDownload,
        Capability::JavascriptEvaluate,
        Capability::IntentExecute,
        Capability::VisionAssist,
        Capability::ArtifactRead,
        Capability::ArtifactCapture,
        Capability::RecoveryRead,
        Capability::RecoveryWrite,
    ];

    /// Every operation, against a principal holding nothing. Not one may
    /// produce a prompt: a prompt implies a human could make it work.
    #[test]
    fn a_principal_holding_nothing_is_never_prompted_for_anything() {
        for operation in EVERY_OPERATION {
            let decision = decide(
                EscalationRequest::new(*operation),
                &[],
                SessionPolicyGates::default(),
            );
            assert!(
                matches!(decision, Escalation::Denied { .. })
                    || decision == Escalation::AlreadyPermitted,
                "{operation:?} produced {decision:?} for a principal holding nothing"
            );
            assert!(
                !decision.should_prompt(),
                "{operation:?} would prompt a human who cannot be helped by answering"
            );
        }
    }

    /// The claim in one assertion: approval never turns a missing capability
    /// into a permitted operation.
    #[test]
    fn approval_cannot_mint_a_capability() {
        for operation in EVERY_OPERATION {
            let decision = decide(
                EscalationRequest::new(*operation),
                &[],
                SessionPolicyGates::default(),
            );
            if let Escalation::Denied { .. } = decision {
                assert!(
                    !decision.permits_after_approval(),
                    "{operation:?} would proceed after approval despite a missing capability"
                );
            }
        }
    }

    /// The one thing a human may do: lift a session-policy gate over authority
    /// the token already carries.
    #[test]
    fn a_held_but_gated_capability_is_the_only_thing_a_human_can_lift() {
        let gated = decide(
            EscalationRequest::new(InterfaceOperation::SubmitCommand),
            EVERY_CAPABILITY,
            SessionPolicyGates {
                vision_assist: false,
            },
        );
        // SubmitCommand does not require vision, so it is already permitted.
        assert_eq!(gated, Escalation::AlreadyPermitted);

        let vision_gated = decide(
            EscalationRequest::with_vision(InterfaceOperation::SubmitCommand),
            EVERY_CAPABILITY,
            SessionPolicyGates {
                vision_assist: false,
            },
        );
        assert_eq!(
            vision_gated,
            Escalation::AskUser {
                capability: Capability::VisionAssist
            },
            "a held-but-gated vision operation should be the prompt case"
        );
        assert!(vision_gated.should_prompt());
        assert!(vision_gated.permits_after_approval());

        let ungated = decide(
            EscalationRequest::with_vision(InterfaceOperation::SubmitCommand),
            EVERY_CAPABILITY,
            SessionPolicyGates {
                vision_assist: true,
            },
        );
        assert_eq!(ungated, Escalation::AlreadyPermitted);
    }

    /// Holding the session gate open does not substitute for the capability.
    /// Without this, an ACP client could open the gate and reach vision with a
    /// token that never carried `vision:assist`.
    #[test]
    fn an_open_session_gate_does_not_substitute_for_the_capability() {
        let without_vision: Vec<Capability> = EVERY_CAPABILITY
            .iter()
            .copied()
            .filter(|capability| *capability != Capability::VisionAssist)
            .collect();
        let decision = decide(
            EscalationRequest::with_vision(InterfaceOperation::SubmitCommand),
            &without_vision,
            SessionPolicyGates {
                vision_assist: true,
            },
        );
        assert_eq!(
            decision,
            Escalation::Denied {
                missing: Capability::VisionAssist
            },
            "an open session gate stood in for a missing capability"
        );
        assert!(!decision.permits_after_approval());
    }

    /// Every operation the interface defines. Listed rather than derived so a
    /// new operation fails to compile here until someone decides whether an
    /// ACP prompt may escalate it.
    const EVERY_OPERATION: &[InterfaceOperation] = &[
        InterfaceOperation::RuntimeInfo,
        InterfaceOperation::CreateSession,
        InterfaceOperation::ReadSession,
        InterfaceOperation::DeleteSession,
        InterfaceOperation::OpenPage,
        InterfaceOperation::ReadPage,
        InterfaceOperation::ClosePage,
        InterfaceOperation::SubmitCommand,
        InterfaceOperation::CreateCheckpoint,
        InterfaceOperation::ReadCheckpoint,
        InterfaceOperation::RecoverWorkflow,
        InterfaceOperation::ReadArtifact,
        InterfaceOperation::CaptureArtifact,
        InterfaceOperation::SubscribeEvents,
        InterfaceOperation::SubmitJob,
        InterfaceOperation::ReadJob,
        InterfaceOperation::CancelJob,
        InterfaceOperation::IssuePrincipal,
        InterfaceOperation::RevokePrincipal,
    ];
}
