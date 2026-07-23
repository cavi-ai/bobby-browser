use companion_protocol::{BrowserEngine, BrowserIdentity, InteractionPath};
use thiserror::Error;

const REQUIRED_OPERATIONS: [&str; 4] = ["navigate", "inspect", "click", "typeText"];
const NATIVE_INPUT_OPERATIONS: [&str; 2] = ["click", "typeText"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeBrowserOperationProof {
    pub name: String,
    pub interaction_path: InteractionPath,
    pub postcondition_verified: bool,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeBrowserProof {
    pub browser: Option<BrowserIdentity>,
    pub operations: Vec<NativeBrowserOperationProof>,
    pub confirmation_text: String,
    pub evidence: Vec<String>,
    pub redaction_findings: Vec<String>,
    pub elapsed_ms: u64,
    pub deadline_ms: u64,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum NativeBrowserProofError {
    #[error("browser identity is missing")]
    MissingBrowserIdentity,
    #[error("browser identity field is empty: {0}")]
    IncompleteBrowserIdentity(String),
    #[error("browser engine is not Firefox")]
    WrongBrowserEngine,
    #[error("browser name is not exactly Firefox")]
    WrongBrowserName,
    #[error("required verified operation is missing: {0}")]
    MissingVerifiedOperation(String),
    #[error("proof does not contain exactly the required operation records")]
    InvalidOperationSet,
    #[error("required native-input operation did not use engine-native input: {0}")]
    NonNativeInput(String),
    #[error("proof contains {0} redaction finding(s)")]
    RedactionFindings(usize),
    #[error("proof contains sensitive retained evidence")]
    SensitiveEvidence,
    #[error("proof does not contain the exact submission confirmation")]
    UnexpectedConfirmation,
    #[error("proof timing is outside its positive deadline")]
    TimingOutOfBounds,
}

pub fn evaluate_native_browser_proof(
    proof: &NativeBrowserProof,
) -> Result<(), NativeBrowserProofError> {
    let browser = proof
        .browser
        .as_ref()
        .ok_or(NativeBrowserProofError::MissingBrowserIdentity)?;
    if browser.engine != BrowserEngine::Firefox {
        return Err(NativeBrowserProofError::WrongBrowserEngine);
    }
    for (name, value) in [
        ("browserName", browser.browser_name.as_str()),
        ("browserVersion", browser.browser_version.as_str()),
        ("os", browser.os.as_str()),
        ("profileLabel", browser.profile_label.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(NativeBrowserProofError::IncompleteBrowserIdentity(
                name.into(),
            ));
        }
    }
    if browser.browser_name != "Firefox" {
        return Err(NativeBrowserProofError::WrongBrowserName);
    }
    if !proof.redaction_findings.is_empty() {
        return Err(NativeBrowserProofError::RedactionFindings(
            proof.redaction_findings.len(),
        ));
    }
    if proof.evidence.iter().any(|item| sensitive(item)) {
        return Err(NativeBrowserProofError::SensitiveEvidence);
    }
    if proof.confirmation_text != "Submitted" {
        return Err(NativeBrowserProofError::UnexpectedConfirmation);
    }
    if proof.operations.len() > REQUIRED_OPERATIONS.len() {
        return Err(NativeBrowserProofError::InvalidOperationSet);
    }
    if proof.elapsed_ms == 0 || proof.deadline_ms == 0 || proof.elapsed_ms > proof.deadline_ms {
        return Err(NativeBrowserProofError::TimingOutOfBounds);
    }
    for name in REQUIRED_OPERATIONS {
        let operation = proof
            .operations
            .iter()
            .find(|operation| operation.name == name && operation.postcondition_verified)
            .ok_or_else(|| NativeBrowserProofError::MissingVerifiedOperation(name.into()))?;
        if operation.duration_ms == 0 || operation.duration_ms > proof.deadline_ms {
            return Err(NativeBrowserProofError::TimingOutOfBounds);
        }
        if NATIVE_INPUT_OPERATIONS.contains(&name)
            && operation.interaction_path != InteractionPath::EngineNative
        {
            return Err(NativeBrowserProofError::NonNativeInput(name.into()));
        }
    }
    Ok(())
}

fn sensitive(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    [
        "authorization",
        "bearer",
        "password",
        "credential",
        "api-key",
        "api_key",
        "secret",
        "token",
    ]
    .iter()
    .any(|marker| value.contains(marker))
}
