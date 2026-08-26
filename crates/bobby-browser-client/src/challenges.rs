//! Challenge detection and solving types for captchas, MFA, and verification flows.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum ChallengeType {
    RecaptchaV2Checkbox,
    RecaptchaV3,
    TextCaptcha,
    ImageGridCaptcha,
    MfaCodeEntry,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChallengeRegion {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChallengeDetectionHints {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_field_purpose: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DetectChallengeIntent {
    pub purpose: String,
    #[serde(default)]
    pub hints: DetectChallengeHints,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct DetectChallengeHints {
    /// Restrict classification to a page region (CSS pixels, viewport-relative).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<ChallengeRegion>,
    /// Deadline for the whole detection, including provider retries.
    #[serde(default = "default_detect_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for DetectChallengeHints {
    fn default() -> Self {
        Self {
            region: None,
            timeout_ms: default_detect_timeout_ms(),
        }
    }
}

fn default_detect_timeout_ms() -> u64 {
    15_000
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SolveChallengeIntent {
    pub purpose: String,
    #[serde(default)]
    pub hints: SolveChallengeHints,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct SolveChallengeHints {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<ChallengeRegion>,
    #[serde(default = "default_solve_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for SolveChallengeHints {
    fn default() -> Self {
        Self {
            region: None,
            timeout_ms: default_solve_timeout_ms(),
        }
    }
}

fn default_solve_timeout_ms() -> u64 {
    30_000
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SolveStep {
    Click {
        x: f32,
        y: f32,
    },
    TypeText {
        text: String,
        target_field_purpose: String,
    },
    WaitAndReassess {
        ms: u64,
    },
}
