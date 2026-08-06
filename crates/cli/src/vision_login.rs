use std::path::PathBuf;

use anyhow::{Context, Result};
use auth_broker::{AuthDriver, AuthError, AuthProfileId, AuthProgress};
use config::AppConfig;
use node_registry::NodeRegistry;

pub async fn login(config: Option<PathBuf>, name: &str) -> Result<()> {
    let config_path = crate::resolve_config_path(config);
    let config = AppConfig::load(&config_path)
        .with_context(|| format!("failed to load config from {}", config_path.display()))?;
    let registry = NodeRegistry::from_config(&config);
    let driver = registry.auth_driver(name)?;
    let strategy = registry.auth_strategy(name)?;
    let profile = AuthProfileId::new(name.to_owned())?;
    let capabilities = driver.discover(&profile).await?;

    match driver.begin(&profile, strategy).await {
        Ok(AuthProgress::Authenticated(handle)) => {
            let method = handle
                .payload::<String>()
                .map(String::as_str)
                .unwrap_or("none");
            println!("Authenticated vision profile {name} with method {method}");
            Ok(())
        }
        Ok(AuthProgress::Pending(challenge)) => {
            eprintln!("Authentication pending for {name}: {}", challenge.id);
            if let Some(uri) = challenge.verification_uri {
                eprintln!("Verification URL: {uri}");
            }
            if let Some(code) = challenge.user_code {
                eprintln!("User code: {code}");
            }
            anyhow::bail!(
                "multi-step OAuth continuation is not productized; complete login in the harness"
            )
        }
        Err(AuthError::Rejected | AuthError::UnsupportedStrategy) => {
            let advertised = capabilities
                .strategies()
                .map(|item| format!("{item:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "vision authentication for {name} was rejected or unsupported; harness advertises: {advertised}"
            )
        }
        Err(error) => Err(error.into()),
    }
}
