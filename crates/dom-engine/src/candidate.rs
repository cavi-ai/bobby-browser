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
}
