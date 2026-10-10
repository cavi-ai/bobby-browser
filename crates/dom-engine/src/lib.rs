mod accessibility;
mod candidate;
mod resolver;

/// JavaScript native HTML role inference shared by DOM candidate probes.
pub const IMPLICIT_ROLE_SCRIPT: &str = include_str!("implicit_role.js");

pub use candidate::{Candidate, CandidateState};
pub use resolver::{
    rank_candidates, resolve_candidates, ResolutionDecision, ResolutionError, ResolutionPolicy,
};

pub use accessibility::{
    accessibility_alignment_unnecessary, accessibility_candidates, apply_accessibility_alignment,
};
