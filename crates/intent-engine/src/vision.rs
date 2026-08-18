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

    /// Process-local aggregate metrics for this provider boundary. Providers
    /// that are not attached to a runtime registry preserve legacy behavior.
    fn operational_metrics(
        &self,
    ) -> Option<(
        observability::OperationalMetrics,
        observability::ProviderMode,
    )> {
        None
    }

    fn provider_mode(&self) -> observability::ProviderMode {
        observability::ProviderMode::DirectLocal
    }
}

struct InstrumentedVisionAssist {
    inner: std::sync::Arc<dyn VisionAssist>,
    metrics: observability::OperationalMetrics,
}

#[async_trait]
impl VisionAssist for InstrumentedVisionAssist {
    async fn propose(&self, request: VisionProposeRequest) -> Result<VisionProposal, CommandError> {
        self.inner.propose(request).await
    }

    fn operational_metrics(
        &self,
    ) -> Option<(
        observability::OperationalMetrics,
        observability::ProviderMode,
    )> {
        Some((self.metrics.clone(), self.inner.provider_mode()))
    }

    fn provider_mode(&self) -> observability::ProviderMode {
        self.inner.provider_mode()
    }
}

pub fn instrument_vision_assist(
    inner: std::sync::Arc<dyn VisionAssist>,
    metrics: observability::OperationalMetrics,
) -> std::sync::Arc<dyn VisionAssist> {
    std::sync::Arc::new(InstrumentedVisionAssist { inner, metrics })
}

#[derive(Debug, Clone)]
pub struct VisionProposeRequest {
    pub purpose: String,
    pub intent_kind: String,
    pub screenshot_png: Vec<u8>,
    pub stuck: StuckKind,
    /// Optional context block enriching the provider prompt. Structure and
    /// command kinds only — never typed values or page text.
    pub context: Option<VisionPromptContext>,
}

/// Context for a vision prompt: where the page is, what the candidate
/// controls look like, and what recently happened. Every field is
/// structural; the type has nowhere to carry a value.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VisionPromptContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<VisionPromptCandidate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_command_kinds: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VisionPromptCandidate {
    pub role: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinal: Option<u32>,
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
            // Raised for the context block: url + top-K candidates + recent
            // command kinds must fit beside purpose and intent kind, and the
            // budget test proves the block is still bounded.
            max_text_bytes: 4_096,
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
    pub context: Option<VisionPromptContext>,
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
    pub context: Option<VisionPromptContext>,
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
    #[error("vision candidate action index is outside the supplied candidate window")]
    CandidateIndexOutOfBounds,
    #[error("vision click coordinates are outside the supplied crop")]
    CoordinateOutOfBounds,
    #[error("vision result confidence is below the execution floor")]
    ConfidenceBelowFloor,
}

pub fn compile_vision_packet(
    input: VisionPacketInput,
    budget: VisionContextBudget,
) -> Result<VisionTaskPacket, VisionPacketError> {
    let context_bytes = input
        .context
        .as_ref()
        .map(serde_json::to_vec)
        .transpose()
        .map_err(|_| VisionPacketError::TextBudgetExceeded)?
        .map_or(0, |bytes| bytes.len());
    let text_bytes = input
        .purpose
        .len()
        .checked_add(input.intent_kind.len())
        .and_then(|total| total.checked_add(context_bytes))
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
        context: input.context,
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
        VisionAction::TypeText { .. } => "type_text",
        VisionAction::ExtractValue { .. } => "extract_value",
        VisionAction::ClickCandidate { .. } => "click_candidate",
        VisionAction::TypeIntoCandidate { .. } => "type_into_candidate",
        VisionAction::ExtractFromCandidate { .. } => "extract_from_candidate",
        VisionAction::ChallengeSolved => "challenge_solved",
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
        VisionAction::ClickCandidate { index } => {
            validate_candidate_index(packet.context.as_ref(), index)?;
            VisionAction::ClickCandidate { index }
        }
        VisionAction::TypeIntoCandidate { index } => {
            validate_candidate_index(packet.context.as_ref(), index)?;
            VisionAction::TypeIntoCandidate { index }
        }
        VisionAction::ExtractFromCandidate { index } => {
            validate_candidate_index(packet.context.as_ref(), index)?;
            VisionAction::ExtractFromCandidate { index }
        }
        other => other,
    };
    Ok(VisionProposal {
        confidence: result.confidence,
        action,
    })
}

pub(crate) fn validate_candidate_index(
    context: Option<&VisionPromptContext>,
    index: u32,
) -> Result<(), VisionPacketError> {
    let candidate_count = context.map_or(0, |context| context.candidates.len());
    if candidate_count == 0 || index as usize >= candidate_count {
        return Err(VisionPacketError::CandidateIndexOutOfBounds);
    }
    Ok(())
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
    /// Click the candidate at this index in the prompt's candidate list.
    /// The runtime owns spatial grounding: the engine resolves the index to
    /// the candidate it sent and clicks that element directly — no pixel
    /// coordinates cross the wire.
    ClickCandidate {
        index: u32,
    },
    /// Fill the candidate at this index with a runtime-only value. The model
    /// can choose the structural target but cannot receive or return its text.
    TypeIntoCandidate {
        index: u32,
    },
    /// Extract from the candidate at this index. The runtime obtains the
    /// value locally; the provider response carries only the target index.
    ExtractFromCandidate {
        index: u32,
    },
    /// The model reports the challenge it was asked to solve (a captcha or
    /// verification widget) is now in a solved state. Only valid for the
    /// `solveChallenge` intent; carries no payload.
    ChallengeSolved,
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
        VisionAction::ClickCandidate { index } => {
            hasher.update(b"clickCandidate");
            hasher.update(index.to_le_bytes());
        }
        VisionAction::TypeIntoCandidate { index } => {
            hasher.update(b"typeIntoCandidate");
            hasher.update(index.to_le_bytes());
        }
        VisionAction::ExtractFromCandidate { index } => {
            hasher.update(b"extractFromCandidate");
            hasher.update(index.to_le_bytes());
        }
        VisionAction::ChallengeSolved => {
            hasher.update(b"challengeSolved");
        }
    }
    hex::encode(hasher.finalize())
}
