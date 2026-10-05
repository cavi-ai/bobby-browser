//! Explicit operator attestation for an uncertain scheduled effect. Never replay.
use crate::PrincipalId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum JobResolutionDecision {
    EffectObserved,
    EffectAbsent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobResolutionRequest {
    pub decision: JobResolutionDecision,
    pub evidence_sha256: String,
}

impl JobResolutionRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.evidence_sha256.len() != 64
            || !self
                .evidence_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("evidenceSha256 must be a lowercase SHA-256 digest");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobResolutionReceipt {
    pub job_id: String,
    pub decision: JobResolutionDecision,
    pub evidence_sha256: String,
    pub actor: PrincipalId,
    pub resolved_at: DateTime<Utc>,
    /// The runtime records an operator's assertion; it does not verify the effect.
    pub provenance: String,
}

impl JobResolutionReceipt {
    pub fn validate(&self) -> Result<(), &'static str> {
        crate::JobId::parse(self.job_id.clone())
            .map_err(|_| "invalid resolution job identifier")?;
        JobResolutionRequest {
            decision: self.decision,
            evidence_sha256: self.evidence_sha256.clone(),
        }
        .validate()?;
        if self.provenance != "operatorAttested" {
            return Err("invalid resolution provenance");
        }
        Ok(())
    }
}
