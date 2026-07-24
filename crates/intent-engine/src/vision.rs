use async_trait::async_trait;
use sha2::{Digest, Sha256};
use types::CommandError;

use crate::stuck::StuckKind;

/// Minimum confidence required to execute a vision proposal. Below this floor the
/// engine fails closed with `VisionAssistFailed`.
pub const VISION_CONFIDENCE_FLOOR: f32 = 0.75;

#[async_trait]
pub trait VisionAssist: Send + Sync {
    async fn propose(&self, request: VisionProposeRequest) -> Result<VisionProposal, CommandError>;
}

#[derive(Debug, Clone)]
pub struct VisionProposeRequest {
    pub purpose: String,
    pub intent_kind: String,
    pub screenshot_png: Vec<u8>,
    pub stuck: StuckKind,
}

#[derive(Debug, Clone)]
pub struct VisionProposal {
    pub confidence: f32,
    pub action: VisionAction,
}

#[derive(Debug, Clone)]
pub enum VisionAction {
    Click {
        x: f64,
        y: f64,
    },
    TypeText {
        text: String,
    },
    /// A read of a value the caller asked to extract, proposed by reading
    /// the screenshot rather than acting on the page. Only valid in
    /// response to an `ExtractIntent` field escalation; other intents never
    /// produce or accept this action.
    ExtractValue {
        value: String,
    },
}

pub fn proposal_sha256(proposal: &VisionProposal) -> String {
    let mut hasher = Sha256::new();
    hasher.update(proposal.confidence.to_le_bytes());
    match &proposal.action {
        VisionAction::Click { x, y } => {
            hasher.update(b"click");
            hasher.update(x.to_le_bytes());
            hasher.update(y.to_le_bytes());
        }
        VisionAction::TypeText { text } => {
            hasher.update(b"type");
            hasher.update(text.as_bytes());
        }
        VisionAction::ExtractValue { value } => {
            hasher.update(b"extract");
            hasher.update(value.as_bytes());
        }
    }
    hex::encode(hasher.finalize())
}
