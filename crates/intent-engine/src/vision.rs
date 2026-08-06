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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisionContextBudget {
    pub max_text_bytes: usize,
    pub max_image_bytes: usize,
    pub max_image_width: u32,
    pub max_image_height: u32,
}

impl Default for VisionContextBudget {
    fn default() -> Self {
        Self {
            max_text_bytes: 1_024,
            max_image_bytes: 4 * 1_024 * 1_024,
            max_image_width: 4_096,
            max_image_height: 4_096,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisionImageRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub viewport_width: u32,
    pub viewport_height: u32,
}

#[derive(Debug, Clone)]
pub struct VisionPacketInput {
    pub purpose: String,
    pub intent_kind: String,
    pub stuck: StuckKind,
    pub screenshot_png: Vec<u8>,
    pub region: VisionImageRegion,
    pub allowed_actions: Vec<String>,
    pub evidence_digest: String,
}

#[derive(Debug, Clone)]
pub struct VisionTaskPacket {
    pub purpose: String,
    pub intent_kind: String,
    pub stuck: StuckKind,
    pub screenshot_png: Vec<u8>,
    pub region: VisionImageRegion,
    pub allowed_actions: Vec<String>,
    pub evidence_digest: String,
}

#[derive(Debug, Clone)]
pub struct VisionBackendResult {
    pub confidence: f32,
    pub action: VisionAction,
    pub evidence_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VisionPacketError {
    #[error("vision text context exceeds its byte budget")]
    TextBudgetExceeded,
    #[error("vision image exceeds its byte budget")]
    ImageBudgetExceeded,
    #[error("vision image region is invalid or exceeds its dimension budget")]
    InvalidImageRegion,
    #[error("vision evidence digest is invalid")]
    InvalidEvidenceDigest,
    #[error("vision result references different evidence")]
    EvidenceMismatch,
    #[error("vision result action is not allowed for this task")]
    ActionNotAllowed,
    #[error("vision click coordinates are outside the supplied crop")]
    CoordinateOutOfBounds,
    #[error("vision result confidence is below the execution floor")]
    ConfidenceBelowFloor,
}

pub fn compile_vision_packet(
    input: VisionPacketInput,
    budget: VisionContextBudget,
) -> Result<VisionTaskPacket, VisionPacketError> {
    let text_bytes = input
        .purpose
        .len()
        .checked_add(input.intent_kind.len())
        .ok_or(VisionPacketError::TextBudgetExceeded)?;
    if text_bytes > budget.max_text_bytes {
        return Err(VisionPacketError::TextBudgetExceeded);
    }
    if input.screenshot_png.len() > budget.max_image_bytes {
        return Err(VisionPacketError::ImageBudgetExceeded);
    }
    let region = input.region;
    let region_valid = region.width > 0
        && region.height > 0
        && region.width <= budget.max_image_width
        && region.height <= budget.max_image_height
        && region
            .x
            .checked_add(region.width)
            .is_some_and(|right| right <= region.viewport_width)
        && region
            .y
            .checked_add(region.height)
            .is_some_and(|bottom| bottom <= region.viewport_height);
    if !region_valid {
        return Err(VisionPacketError::InvalidImageRegion);
    }
    if input.evidence_digest.len() != 64
        || !input
            .evidence_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(VisionPacketError::InvalidEvidenceDigest);
    }
    Ok(VisionTaskPacket {
        purpose: input.purpose,
        intent_kind: input.intent_kind,
        stuck: input.stuck,
        screenshot_png: input.screenshot_png,
        region,
        allowed_actions: input.allowed_actions,
        evidence_digest: input.evidence_digest,
    })
}

pub fn validate_backend_result(
    packet: &VisionTaskPacket,
    result: VisionBackendResult,
) -> Result<VisionProposal, VisionPacketError> {
    if result.evidence_digest != packet.evidence_digest {
        return Err(VisionPacketError::EvidenceMismatch);
    }
    if !result.confidence.is_finite() || result.confidence < VISION_CONFIDENCE_FLOOR {
        return Err(VisionPacketError::ConfidenceBelowFloor);
    }
    let action_name = match &result.action {
        VisionAction::Click { .. } => "click",
        VisionAction::TypeText { .. } => "typeText",
        VisionAction::ExtractValue { .. } => "extractValue",
    };
    if !packet
        .allowed_actions
        .iter()
        .any(|allowed| allowed == action_name)
    {
        return Err(VisionPacketError::ActionNotAllowed);
    }
    let action = match result.action {
        VisionAction::Click { x, y }
            if x.is_finite()
                && y.is_finite()
                && x >= 0.0
                && y >= 0.0
                && x <= f64::from(packet.region.width)
                && y <= f64::from(packet.region.height) =>
        {
            VisionAction::Click {
                x: x + f64::from(packet.region.x),
                y: y + f64::from(packet.region.y),
            }
        }
        VisionAction::Click { .. } => return Err(VisionPacketError::CoordinateOutOfBounds),
        other => other,
    };
    Ok(VisionProposal {
        confidence: result.confidence,
        action,
    })
}

#[derive(Debug, Clone)]
pub struct VisionProposal {
    pub confidence: f32,
    pub action: VisionAction,
}

/// A cached click proposal for one field purpose. Coordinates and confidence
/// only — a cached proposal can never carry a typed value, because
/// `TypeText` and `ExtractValue` actions are not cacheable.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedProposal {
    pub x: f64,
    pub y: f64,
    pub confidence: f32,
}

/// Proposal cache lookup, implemented by the runtime's context graph. The
/// engine asks before it escalates; a hit skips the screenshot round-trip.
/// Sync like the graph it fronts: lookups are in-memory.
pub trait ProposalLookup: Send + Sync {
    fn proposal_for(&self, page: &types::PageId, purpose: &str) -> Option<CachedProposal>;
    fn drop_proposal(&self, page: &types::PageId, purpose: &str);
    fn record_proposals(&self, page: &types::PageId, proposals: Vec<(String, CachedProposal)>);
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
