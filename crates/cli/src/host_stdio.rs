//! Shared owner and credential setup for stdio protocol entrypoints.
use crate::{
    load_managed_vision_token, onboarding, resolve_bootstrap_path, resolve_config_path,
    runtime_scopes, VisionSpawnPolicy,
};
use anyhow::Result;
use std::path::PathBuf;

pub(crate) enum Protocol {
    Mcp,
    Acp,
}

pub(crate) async fn run(
    protocol: Protocol,
    bootstrap_env: Option<PathBuf>,
    config: Option<PathBuf>,
    policy: VisionSpawnPolicy,
) -> Result<()> {
    let bootstrap_path = resolve_bootstrap_path(bootstrap_env)?;
    load_managed_vision_token(&bootstrap_path, "BOBBY_VISION_TOKEN")?;
    let config_path = resolve_config_path(config);
    let origin =
        runtime_scopes::ensure_owner(config_path.clone(), bootstrap_path.clone(), policy).await?;
    unsafe {
        std::env::set_var("BOBBY_RUNTIME_URL", origin);
    }
    match protocol {
        Protocol::Mcp => onboarding::exec_mcp_stdio(&bootstrap_path, &config_path),
        Protocol::Acp => onboarding::exec_acp_stdio(&bootstrap_path, &config_path),
    }
}
