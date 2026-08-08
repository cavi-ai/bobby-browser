use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateState {
    pub attached: bool,
    pub visible: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    pub css: Option<String>,
    pub test_id: Option<String>,
    pub role: Option<String>,
    pub name: Option<String>,
    pub label: Option<String>,
    pub text: String,
    pub attributes: BTreeMap<String, String>,
    pub state: CandidateState,
    /// Frame hops (outermost first) when the candidate was gathered inside
    /// an iframe; empty for main-frame candidates. Stamped at gather time so
    /// the action path can re-resolve through the same frames.
    pub frame_path: Vec<Box<types::TargetSpec>>,
}
