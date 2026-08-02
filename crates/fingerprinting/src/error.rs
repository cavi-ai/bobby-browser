use thiserror::Error;

/// Failure while applying a fingerprint plan to a host browsing context.
#[derive(Debug, Error)]
pub enum FingerprintApplyError {
    #[error("fingerprint apply failed: {0}")]
    Host(String),
    #[error("fingerprint profile is inconsistent: {0}")]
    Inconsistent(String),
}
