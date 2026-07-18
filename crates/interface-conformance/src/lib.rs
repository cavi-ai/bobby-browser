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
pub const CANONICAL_EVENT_ORDER: [&str; 9] = [
    "navigation.completed",
    "upload.completed",
    "checkpoint.saved",
    "boundary.completed",
    "checkpoint.saved",
    "boundary.completed",
    "screenshot.verified",
    "recovery.inspected",
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
    pub checkpoint_id: String,
    pub workflow_id: String,
    pub boundary_command_id: String,
    pub recovery_status: String,
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
        || proof.checkpoint_lineage.boundary != "boundary"
        || uuid::Uuid::parse_str(&proof.checkpoint_lineage.checkpoint_id).is_err()
        || uuid::Uuid::parse_str(&proof.checkpoint_lineage.workflow_id).is_err()
        || uuid::Uuid::parse_str(&proof.checkpoint_lineage.boundary_command_id).is_err()
        || !matches!(
            proof.checkpoint_lineage.recovery_status.as_str(),
            "resumed" | "needsReconciliation"
        )
    {
        return Err("Rust SDK proof lacked real normalized observations");
    }
    Ok(proof)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_proof() -> CanonicalProof {
        CanonicalProof {
            outcome_status: "completed".into(),
            evidence: ["navigation", "upload", "screenshot", "download"]
                .into_iter()
                .map(|kind| EvidenceProof {
                    kind: kind.into(),
                    sha256: "a".repeat(64),
                    size: 1,
                })
                .collect(),
            authorization: AuthorizationProof {
                allowed: CANONICAL_ALLOWED.map(str::to_owned).to_vec(),
                denied: DenialProof {
                    capability: "session:read".into(),
                    status: 403,
                },
            },
            event_ordering: CANONICAL_EVENT_ORDER.map(str::to_owned).to_vec(),
            checkpoint_lineage: CheckpointLineage {
                boundary: "boundary".into(),
                replayed: false,
                checkpoint_id: uuid::Uuid::new_v4().to_string(),
                workflow_id: uuid::Uuid::new_v4().to_string(),
                boundary_command_id: uuid::Uuid::new_v4().to_string(),
                recovery_status: "needsReconciliation".into(),
            },
        }
    }

    #[test]
    fn rejects_reordered_observed_event_batch() {
        let mut proof = valid_proof();
        proof.event_ordering.swap(1, 2);
        assert!(validate_canonical_proof(proof).is_err());
    }

    #[test]
    fn rejects_missing_checkpoint_lineage() {
        let mut proof = valid_proof();
        proof.checkpoint_lineage.checkpoint_id.clear();
        assert!(validate_canonical_proof(proof).is_err());
    }

    #[test]
    fn rejects_implicit_boundary_replay() {
        let mut proof = valid_proof();
        proof.checkpoint_lineage.replayed = true;
        proof.checkpoint_lineage.recovery_status = "restarted".into();
        assert!(validate_canonical_proof(proof).is_err());
    }
}
