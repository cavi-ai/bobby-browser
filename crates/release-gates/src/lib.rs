pub mod cli;
pub mod manifest;
pub mod policy;
pub mod process;
pub mod result;
pub mod security;

#[cfg(unix)]
mod persistence;

pub use manifest::{
    ManifestError, ReleaseManifest, SecurityManifest, DEFAULT_CHECK_TIMEOUT_SECS,
    DEFAULT_MAX_OUTPUT_BYTES, MANIFEST_SCHEMA_VERSION,
};
pub use policy::{evaluate, CertificationVerdict, PolicyError};
pub use process::{run_process, ProcessFailure, ProcessOutcome, ProcessSpec};
pub use result::{GateObservation, GateResult, GateStatus, ResultError, RESULT_SCHEMA_VERSION};
pub use security::{
    security_catalog_sha256, CargoTestProof, ProcessRunner, SecurityCheck, SecurityGate,
    TokioProcessRunner,
};
