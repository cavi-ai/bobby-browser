use std::path::Path;

use toml_edit::{value, DocumentMut, Item, Table};

use crate::{VisionAcpProfile, VisionAuthKind, VisionConfig, VisionProviderConfig};

/// Error returned when surgically writing vision settings to a TOML file fails.
#[derive(Debug)]
pub enum ConfigWriteError {
    Io(std::io::Error),
    Parse(toml_edit::TomlError),
    Invalid(String),
}

impl std::fmt::Display for ConfigWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigWriteError::Io(err) => write!(f, "failed to write config file: {err}"),
            ConfigWriteError::Parse(err) => write!(f, "failed to parse config file: {err}"),
            ConfigWriteError::Invalid(reason) => write!(f, "invalid config: {reason}"),
        }
    }
}

impl std::error::Error for ConfigWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigWriteError::Io(err) => Some(err),
            ConfigWriteError::Parse(err) => Some(err),
            ConfigWriteError::Invalid(_) => None,
        }
    }
}

const DEFAULT_LOOPBACK_ENDPOINT: &str = "http://127.0.0.1:9100/vision";
const DEFAULT_VISION_TOKEN_ENV: &str = "BOBBY_VISION_TOKEN";

/// Fill missing loopback vision defaults on an in-memory [`VisionConfig`].
///
/// Used before persisting config during `--vision` setup; does not write secrets.
pub fn ensure_loopback_vision_defaults(config: &mut VisionConfig) {
    if config.endpoint_url.is_none() {
        config.endpoint_url = Some(DEFAULT_LOOPBACK_ENDPOINT.to_string());
    }
    if config.token_env.is_none() {
        config.token_env = Some(DEFAULT_VISION_TOKEN_ENV.to_string());
    }
}

/// Upsert vision platform settings into `path`, preserving unrelated tables.
///
/// The HTTP vision endpoint is written under `[nodes.vision]` (`kind = "vision"`)
/// so runtime resolution goes through `NodeRegistry` without a dual-truth
/// `[vision].endpoint_url`. Provider profiles stay under `[vision.providers.*]`.
///
/// Only env var names are written — never secret values.
pub fn upsert_vision_platform(
    path: &Path,
    endpoint_url: &str,
    token_env: &str,
    provider: &str,
    profile: &VisionProviderConfig,
) -> Result<(), ConfigWriteError> {
    let mut doc = if path.exists() {
        let text = std::fs::read_to_string(path).map_err(ConfigWriteError::Io)?;
        text.parse::<DocumentMut>()
            .map_err(ConfigWriteError::Parse)?
    } else {
        DocumentMut::new()
    };

    let vision = ensure_table(doc.as_table_mut(), "vision")?;
    // Prefer [nodes.vision] for the HTTP endpoint; drop legacy dual-truth keys.
    vision.remove("endpoint_url");
    vision.remove("token_env");
    vision["provider"] = value(provider);

    let providers = ensure_table(vision, "providers")?;
    let provider_table = ensure_table(providers, provider)?;

    provider_table["base_url"] = value(&profile.base_url);
    provider_table["model"] = value(&profile.model);
    if let Some(api_key_env) = &profile.api_key_env {
        provider_table["api_key_env"] = value(api_key_env);
    } else {
        provider_table.remove("api_key_env");
    }

    let nodes = ensure_table(doc.as_table_mut(), "nodes")?;
    let node = ensure_table(nodes, "vision")?;
    node["kind"] = value("vision");
    node["endpoint_url"] = value(endpoint_url);
    node["token_env"] = value(token_env);

    std::fs::write(path, doc.to_string()).map_err(ConfigWriteError::Io)?;
    Ok(())
}

/// Upsert an ACP harness profile. Only the executable, arguments, and auth
/// strategy are persisted; provider credentials remain owned by the harness.
pub fn upsert_vision_acp_profile(
    path: &Path,
    profile_name: &str,
    profile: &VisionAcpProfile,
) -> Result<(), ConfigWriteError> {
    let mut doc = if path.exists() {
        std::fs::read_to_string(path)
            .map_err(ConfigWriteError::Io)?
            .parse::<DocumentMut>()
            .map_err(ConfigWriteError::Parse)?
    } else {
        DocumentMut::new()
    };
    let vision = ensure_table(doc.as_table_mut(), "vision")?;
    vision["backend"] = value("acp");
    vision["profile"] = value(profile_name);
    let profiles = ensure_table(vision, "acp_profiles")?;
    let selected = ensure_table(profiles, profile_name)?;
    selected["command"] = value(&profile.command);
    let mut args = toml_edit::Array::new();
    for arg in &profile.args {
        args.push(arg);
    }
    selected["args"] = value(args);
    selected["auth"] = value(match profile.auth {
        VisionAuthKind::Advertised => "advertised",
        VisionAuthKind::OAuthAuthorizationCode => "oauth-authorization-code",
        VisionAuthKind::OAuthDeviceCode => "oauth-device-code",
        VisionAuthKind::Environment => "environment",
        VisionAuthKind::ExistingSession => "existing-session",
        VisionAuthKind::None => "none",
    });
    std::fs::write(path, doc.to_string()).map_err(ConfigWriteError::Io)
}

fn ensure_table<'a>(parent: &'a mut Table, key: &str) -> Result<&'a mut Table, ConfigWriteError> {
    if parent.contains_key(key) && !parent[key].is_table() {
        return Err(ConfigWriteError::Invalid(format!(
            "{key} must be a TOML table"
        )));
    }
    parent
        .entry(key)
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| ConfigWriteError::Invalid(format!("{key} must be a TOML table")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppConfig;

    #[test]
    fn upsert_vision_platform_preserves_unrelated_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[server]\nhost = \"127.0.0.1\"\nport = 7777\n\n[browser]\nheadless = true\n",
        )
        .unwrap();
        let profile = VisionProviderConfig {
            base_url: "http://127.0.0.1:1234/v1".into(),
            model: "local-model".into(),
            api_key_env: None,
        };
        upsert_vision_platform(
            &path,
            "http://127.0.0.1:9100/vision",
            "BOBBY_VISION_TOKEN",
            "lmstudio",
            &profile,
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("host = \"127.0.0.1\""));
        assert!(text.contains("[vision.providers.lmstudio]"));
        assert!(text.contains("[nodes.vision]"));
        assert!(text.contains("kind = \"vision\""));
        let loaded = AppConfig::load(&path).unwrap();
        assert_eq!(loaded.vision.provider.as_deref(), Some("lmstudio"));
        assert!(loaded.vision.endpoint_url.is_none());
        assert_eq!(
            loaded.nodes.get("vision").map(|n| n.endpoint_url.as_str()),
            Some("http://127.0.0.1:9100/vision")
        );
    }

    #[test]
    fn upsert_rejects_non_table_vision_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "vision = \"not-a-table\"\n").unwrap();
        let profile = VisionProviderConfig {
            base_url: "http://127.0.0.1:1234/v1".into(),
            model: "local-model".into(),
            api_key_env: None,
        };
        let err = upsert_vision_platform(
            &path,
            "http://127.0.0.1:9100/vision",
            "BOBBY_VISION_TOKEN",
            "lmstudio",
            &profile,
        )
        .expect_err("non-table vision must fail");
        assert!(err.to_string().contains("table"));
    }

    #[test]
    fn ensure_loopback_vision_defaults_fills_missing_fields_only() {
        let mut config = VisionConfig {
            endpoint_url: Some("http://custom/vision".into()),
            token_env: None,
            ..VisionConfig::default()
        };
        ensure_loopback_vision_defaults(&mut config);
        assert_eq!(config.endpoint_url.as_deref(), Some("http://custom/vision"));
        assert_eq!(config.token_env.as_deref(), Some("BOBBY_VISION_TOKEN"));

        let mut empty = VisionConfig::default();
        ensure_loopback_vision_defaults(&mut empty);
        assert_eq!(
            empty.endpoint_url.as_deref(),
            Some("http://127.0.0.1:9100/vision")
        );
        assert_eq!(empty.token_env.as_deref(), Some("BOBBY_VISION_TOKEN"));
    }
}
