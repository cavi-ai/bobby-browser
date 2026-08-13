//! Challenge detection and solving types for captchas, MFA, and verification flows.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChallengeType {
    RecaptchaV2Checkbox,
    RecaptchaV3,
    TextCaptcha,
    ImageGridCaptcha,
    MfaCodeEntry,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChallengeDetection {
    pub challenge_type: ChallengeType,
    pub confidence: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<ChallengeRegion>,
    #[serde(default)]
    pub blocking: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hints: Option<ChallengeDetectionHints>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ChallengeRegion {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChallengeDetectionHints {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_field_purpose: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SolveChallengeIntent {
    pub purpose: String,
    #[serde(default = "default_solve_hints")]
    pub hints: SolveChallengeHints,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SolveChallengeHints {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<ChallengeRegion>,
    #[serde(default = "default_solve_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_solve_hints() -> SolveChallengeHints {
    SolveChallengeHints::default()
}

fn default_solve_timeout_ms() -> u64 {
    30_000
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SolveStep {
    Click { x: f32, y: f32 },
    TypeText { text: String, target_field_purpose: String },
    WaitAndReassess { ms: u64 },
}
