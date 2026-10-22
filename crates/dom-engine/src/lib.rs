mod accessibility;
mod candidate;
mod page_dom;
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
    static SCRIPT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SCRIPT.get_or_init(|| {
        include_str!("candidate_gather.js")
            .replace("__CONTROL_LABEL_FUNCTION__", control_label_function())
    })
}

/// Associated label text without the labelled control's own value or options.
pub fn control_label_function() -> &'static str {
    include_str!("control_label.js")
        .strip_prefix("export ")
        .expect("control label module exports its page function")
}

pub use page_dom::{
    composed_query_all_function, composed_query_function, ELEMENT_VISIBILITY_SCRIPT,
};
pub use shadow_segment::{
    exact_target_selector, resolve_shadow_segment, shadow_probe_error, shadow_segment_descriptor,
    shadow_segment_function,
};
