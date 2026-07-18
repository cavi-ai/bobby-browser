use serde::{Deserialize, Serialize};

pub mod live;

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
pub const CANONICAL_ALLOWED: [&str; 4] = [
    "page:write",
    "file:upload",
    "artifact:capture",
    "file:download",
];
pub const CANONICAL_EVENT_ORDER: [&str; 6] = [
    "navigation.completed",
    "upload.completed",
    "boundary.completed",
    "screenshot.verified",
    "checkpoint.saved",
    "events.read",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceProof {
    pub kind: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CanonicalProof {
    pub outcome_status: String,
    pub evidence: Vec<EvidenceProof>,
    pub authorization: AuthorizationProof,
    pub event_ordering: Vec<String>,
    pub checkpoint_lineage: CheckpointLineage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizationProof {
    pub allowed: Vec<String>,
    pub denied: DenialProof,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DenialProof {
    pub capability: String,
    pub status: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointLineage {
    pub boundary: String,
    pub replayed: bool,
}

pub trait RustSdkScenarioDriver {
    fn execute(&mut self, steps: &[&str]) -> CanonicalProof;
}

pub fn run_canonical_scenario(
    driver: &mut impl RustSdkScenarioDriver,
) -> Result<CanonicalProof, &'static str> {
    validate_canonical_proof(driver.execute(&CANONICAL_STEPS))
}

pub fn validate_canonical_proof(proof: CanonicalProof) -> Result<CanonicalProof, &'static str> {
    if proof.outcome_status != "completed"
        || proof.evidence.len() != 4
        || proof.evidence.iter().any(|item| {
            item.size == 0
                || item.sha256.len() != 64
                || !item.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        || !matches!(proof.authorization.denied.status, 401 | 403)
        || proof
            .evidence
            .iter()
            .map(|item| item.kind.as_str())
            .collect::<Vec<_>>()
            != ["navigation", "upload", "screenshot", "download"]
        || proof
            .authorization
            .allowed
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            != CANONICAL_ALLOWED
        || proof.authorization.denied.capability != "session:read"
        || proof.authorization.denied.status != 403
        || proof
            .event_ordering
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            != CANONICAL_EVENT_ORDER
        || proof.checkpoint_lineage.replayed
    {
        return Err("Rust SDK proof lacked real normalized observations");
    }
    Ok(proof)
}
