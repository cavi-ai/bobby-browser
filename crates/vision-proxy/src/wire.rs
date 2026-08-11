use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProposeRequest {
    pub purpose: String,
    pub intent_kind: String,
    pub stuck: String,
    pub screenshot_png: String,
    /// Optional context block: page url, candidate controls, recent command
    /// kinds. Structure only; rendered into the upstream prompt.
    #[serde(default)]
    pub context: Option<ProposeContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProposeContext {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub candidates: Vec<ProposeContextCandidate>,
    #[serde(default)]
    pub recent_command_kinds: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProposeContextCandidate {
    pub role: String,
    pub name: String,
    #[serde(default)]
    pub ordinal: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProposeResponse {
    pub confidence: f32,
    pub action: VisionAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum VisionAction {
    Click {
        x: f64,
        y: f64,
    },
    TypeText {
        text: String,
    },
    ExtractValue {
        value: String,
    },
    /// Click the candidate at this index in the prompt's candidate list;
    /// the runtime resolves the element and owns spatial grounding.
    ClickCandidate {
        index: u32,
    },
    TypeIntoCandidate {
        index: u32,
    },
    ExtractFromCandidate {
        index: u32,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtractRequest {
    pub schema: serde_json::Value,
    pub content: String,
    pub purpose: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtractResponse {
    pub value: serde_json::Value,
}
