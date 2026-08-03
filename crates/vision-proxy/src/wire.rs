use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProposeRequest {
    pub purpose: String,
    pub intent_kind: String,
    pub stuck: String,
    pub screenshot_png: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposeResponse {
    pub confidence: f32,
    pub action: VisionAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum VisionAction {
    Click { x: f64, y: f64 },
    TypeText { text: String },
    ExtractValue { value: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtractRequest {
    pub schema: serde_json::Value,
    pub content: String,
    pub purpose: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractResponse {
    pub value: serde_json::Value,
}
