mod accessibility;
mod candidate;
mod resolver;

pub use candidate::{Candidate, CandidateState};
pub use resolver::{
    rank_candidates, resolve_candidates, ResolutionDecision, ResolutionError, ResolutionPolicy,
};

pub use accessibility::{
    accessibility_alignment_unnecessary, accessibility_candidates, apply_accessibility_alignment,
};
