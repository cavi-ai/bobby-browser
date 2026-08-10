pub mod auth;
pub mod data_collector;
pub mod mlx;
pub mod ollama;
pub mod openai;
pub mod server;
pub mod upstream;
pub mod validate;
pub mod wire;

pub use data_collector::{DataCollectorConfig, VisionDataCollector};
pub use mlx::{
    mlx_upstream_from_env, MlxConfigError, MlxUpstream,
    DEFAULT_BASE_URL as MLX_DEFAULT_BASE_URL, DEFAULT_MODEL as MLX_DEFAULT_MODEL,
};
pub use ollama::{
    ollama_upstream_from_env, OllamaConfigError, OllamaUpstream,
    DEFAULT_BASE_URL as OLLAMA_DEFAULT_BASE_URL, DEFAULT_MODEL as OLLAMA_DEFAULT_MODEL,
};
pub use openai::{
    openai_upstream_from_env, OpenAiConfigError, OpenAiUpstream,
    DEFAULT_BASE_URL as OPENAI_DEFAULT_BASE_URL, DEFAULT_MODEL as OPENAI_DEFAULT_MODEL,
};
pub use server::{router, serve, AppState, ProxyConfig, UpstreamKind};
pub use upstream::{ExtractInput, ProposeInput, Upstream, UpstreamError};
pub use validate::{validate_extract, validate_proposal, ValidateError};
pub use wire::{ExtractRequest, ExtractResponse, ProposeRequest, ProposeResponse, VisionAction};
