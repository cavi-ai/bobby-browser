//! What an ACP permission prompt is allowed to change.
//!
//! A human click must never mint authority the bearer token does not carry, so
//! whether to prompt at all is decided here, before any prompt is sent:
//!
//! - Capability not held: [`Escalation::Denied`], no prompt sent.
//! - Held and session policy permits it: [`Escalation::AlreadyPermitted`].
//! - Held but session policy gates it: [`Escalation::AskUser`], the only case
//!   that reaches a human.
//!
//! Double-gate: capability and session policy. A human can move only the
//! second.

use types::{Capability, InterfaceOperation};

/// Whether an ACP permission prompt may be sent, and what it could change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Escalation {
    /// The principal already holds the capability and policy permits it.
    AlreadyPermitted,
    /// The principal holds the capability; session policy gates it. A human
    /// may lift that gate.
    AskUser { capability: Capability },
    /// The principal does not hold the capability. No prompt is sent.
    Denied { missing: Capability },
}

/// Session policy flags an ACP prompt could lift, mirroring
/// `types::ExecutionPolicy`.
///
/// `javascriptEvaluation`, `fingerprint`, and `humanize` are deliberately
/// absent: each is a session-creation decision, so a mid-session prompt
/// flipping one would widen the session past what its creator asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionPolicyGates {
    pub vision_assist: bool,
}

/// What an ACP client is asking to do.
///
/// `requires_vision` stays separate from `operation`: no `InterfaceOperation`
/// names `vision:assist`, because the double-gate lives in `intent-engine`
/// where a stuck intent escalates.
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
    // Checked first and returned immediately so no later branch can turn a
    // missing capability into a prompt.
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
    /// affirmatively. `Denied` answers `false` regardless of the answer.
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

    /// A principal holding nothing is never prompted for any operation.
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

    /// Approval never turns a missing capability into a permitted operation.
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

    /// A human may only lift a session-policy gate over a held capability.
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

    /// Listed rather than derived so a new operation fails to compile here
    /// until someone decides whether an ACP prompt may escalate it.
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
