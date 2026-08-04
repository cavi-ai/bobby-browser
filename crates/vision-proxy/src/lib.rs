pub mod auth;
pub mod openai;
pub mod server;
pub mod upstream;
pub mod validate;
pub mod wire;

pub use openai::{
    openai_upstream_from_env, OpenAiConfigError, OpenAiUpstream, DEFAULT_BASE_URL, DEFAULT_MODEL,
};
pub use server::{router, serve, AppState, ProxyConfig};
pub use upstream::{ExtractInput, ProposeInput, Upstream, UpstreamError};
pub use validate::{validate_extract, validate_proposal, ValidateError};
pub use wire::{ExtractRequest, ExtractResponse, ProposeRequest, ProposeResponse, VisionAction};
