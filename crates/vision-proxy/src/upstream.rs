use async_trait::async_trait;

use crate::wire::{ExtractResponse, ProposeResponse};

#[async_trait]
pub trait Upstream: Send + Sync {
    async fn propose(&self, input: ProposeInput) -> Result<ProposeResponse, UpstreamError>;
    async fn extract(&self, input: ExtractInput) -> Result<ExtractResponse, UpstreamError>;
}

pub struct ProposeInput {
    pub purpose: String,
    pub intent_kind: String,
    pub stuck: String,
    pub screenshot_png_b64: String,
    pub context: Option<crate::wire::ProposeContext>,
}

pub struct ExtractInput {
    pub schema: serde_json::Value,
    pub content: String,
    pub purpose: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    #[error("upstream transport failed: {0}")]
    Transport(String),
    #[error("upstream rejected the request: {0}")]
    Rejected(String),
    #[error("upstream returned an invalid payload: {0}")]
    Invalid(String),
}
