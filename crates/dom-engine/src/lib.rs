mod candidate;
mod resolver;

pub use candidate::{Candidate, CandidateState};
pub use resolver::{resolve_candidates, ResolutionDecision, ResolutionError, ResolutionPolicy};
