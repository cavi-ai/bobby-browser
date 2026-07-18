use serde::{Deserialize, Serialize};

pub const CANONICAL_STEPS: [&str; 10] = [
    "runtime.info",
    "session.create",
    "page.open",
    "command.navigate",
    "command.upload",
    "command.boundary",
    "artifact.verify",
    "checkpoint.save",
    "recovery.inspect",
    "events.read",
];

pub const NEGATIVE_CAPABILITY_MATRIX: [(&str, &str); 10] = [
    ("runtime.info", "session:read"),
    ("session.create", "session:write"),
    ("page.open", "page:write"),
    ("command.navigate", "page:write"),
    ("command.upload", "file:upload"),
    ("command.boundary", "page:write"),
    ("artifact.verify", "artifact:read"),
    ("checkpoint.save", "recovery:write"),
    ("recovery.inspect", "recovery:read"),
    ("events.read", "session:read"),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceProof {
    pub kind: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CanonicalProof {
    pub outcome_status: String,
    pub evidence: Vec<EvidenceProof>,
    pub authorization: Vec<String>,
    pub event_ordering: Vec<String>,
    pub checkpoint_lineage: Vec<String>,
    pub implicit_boundary_replay: bool,
}

pub fn expected_proof() -> CanonicalProof {
    CanonicalProof {
        outcome_status: "completed".into(),
        evidence: [
            ('a', "navigation"),
            ('b', "upload"),
            ('c', "screenshot"),
            ('d', "download"),
        ]
        .into_iter()
        .map(|(hash, kind)| EvidenceProof {
            kind: kind.into(),
            sha256: hash.to_string().repeat(64),
        })
        .collect(),
        authorization: [
            "allow:session:write",
            "allow:page:write",
            "allow:artifact:capture",
            "deny:javascript:evaluate",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        event_ordering: ["command.accepted", "command.completed", "checkpoint.saved"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        checkpoint_lineage: ["checkpoint-1", "attempt-1", "boundary-1"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        implicit_boundary_replay: false,
    }
}

pub trait RustSdkScenarioDriver {
    fn execute(&mut self, steps: &[&str]) -> CanonicalProof;
}

pub fn run_canonical_scenario(
    driver: &mut impl RustSdkScenarioDriver,
) -> Result<CanonicalProof, &'static str> {
    let proof = driver.execute(&CANONICAL_STEPS);
    if proof != expected_proof() {
        return Err("Rust SDK proof weakened the canonical contract");
    }
    Ok(proof)
}
