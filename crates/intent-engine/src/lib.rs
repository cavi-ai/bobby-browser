mod compiler;
mod corpus;
mod engine;
mod http_vision;
mod stuck;
mod verify;
mod vision;

pub use compiler::{compile_intent, CompileError, ExtractFieldPlan, IntentPlan};
pub use corpus::{CorpusCandidate, CorpusHealth, CorpusRecord, ResolvedElement, VisionCorpus};
pub use engine::{IntentBrowser, IntentEngine, IntentOutcome, VisionContext};
pub use http_vision::{HttpVisionAssist, StructuredExtractRequest, StructuredExtractor};
pub use stuck::{never_escalates, StuckKind};
pub use verify::{compatible, execution_record, summarize_target, verify_fill};
pub use vision::{
    compile_vision_packet, instrument_vision_assist, proposal_sha256, validate_backend_result,
    CachedProposal, ProposalLookup, VisionAction, VisionAssist, VisionBackendResult,
    VisionContextBudget, VisionImageRegion, VisionPacketError, VisionPacketInput,
    VisionPromptCandidate, VisionPromptContext, VisionProposal, VisionProposeRequest,
    VisionTaskPacket, VISION_CONFIDENCE_FLOOR,
};
