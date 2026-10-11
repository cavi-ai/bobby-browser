mod accessibility;
mod candidate;
mod resolver;
mod shadow_segment;

pub use candidate::{Candidate, CandidateState};
pub use resolver::{
    rank_candidates, resolve_candidates, ResolutionDecision, ResolutionError, ResolutionPolicy,
};

pub use accessibility::{
    accessibility_alignment_unnecessary, accessibility_candidates, apply_accessibility_alignment,
};

/// In-page collection shared by Chromium and Firefox transport adapters.
pub fn candidate_gather_function() -> &'static str {
    include_str!("candidate_gather.js")
}

pub use shadow_segment::{
    exact_target_selector, resolve_shadow_segment, shadow_probe_error, shadow_segment_descriptor,
    shadow_segment_function,
};
