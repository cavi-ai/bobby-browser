//! `bobby vision connect` — configure a named vision provider profile in config.toml.

use std::io::{BufRead, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Result};
use config::{
    upsert_vision_acp_profile, upsert_vision_platform, VisionAcpProfile, VisionAuthKind,
    VisionProviderConfig,
};

pub const LOOPBACK_ENDPOINT: &str = "http://127.0.0.1:9100/vision";
pub const VISION_TOKEN_ENV: &str = "BOBBY_VISION_TOKEN";

/// CLI flags for `bobby vision connect`.
#[derive(Debug, Clone)]
pub struct ConnectOpts {
    pub config: Option<PathBuf>,
    pub provider: Option<String>,
    pub backend: String,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub auth: String,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key_env: Option<String>,
    pub yes: bool,
    pub activate: bool,
    pub download_model: bool,
}

impl Default for ConnectOpts {
    fn default() -> Self {
        Self {
            config: None,
            provider: None,
            backend: "direct".into(),
            command: None,
            args: Vec::new(),
            auth: "advertised".into(),
            base_url: None,
            model: None,
            api_key_env: None,
            yes: false,
            activate: false,
            download_model: false,
        }
    }
}

struct ResolvedProfile {
    provider_name: String,
    profile: VisionProviderConfig,
}

/// Configure vision provider settings and persist them to config.toml.
pub fn connect(opts: ConnectOpts) -> Result<()> {
    let config_path = crate::resolve_config_path(opts.config.clone());
    if opts.backend.eq_ignore_ascii_case("acp") {
        if opts.activate || opts.download_model {
            anyhow::bail!("--activate is available only for direct vision providers");
        }
        let name = opts
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("ACP setup requires --provider as the profile name"))?;
        let command = opts
            .command
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("ACP setup requires --command"))?;
        let auth = parse_auth(&opts.auth)?;
        upsert_vision_acp_profile(
            &config_path,
            name,
            &VisionAcpProfile {
                command: command.to_owned(),
                args: opts.args,
                auth,
            },
        )
        .map_err(|error| anyhow!("{error}"))?;
        println!(
            "Wrote ACP vision profile {name:?} to {}",
            config_path.display()
        );
        eprintln!(
            "Configured auth strategy {auth:?}; at runtime Bobby calls harness authenticate via auth-broker (no Keychain access)."
        );
        eprintln!(
            "No provider token was stored here. Unmatched harness auth methods fail closed; multi-step OAuth continue is not productized yet."
        );
        return Ok(());
    }
    if !opts.backend.eq_ignore_ascii_case("direct") {
        anyhow::bail!(
            "unknown vision backend {:?}; expected direct or acp",
            opts.backend
        );
    }
    let resolved = if opts.yes {
        resolve_non_interactive(&opts)?
    } else {
        resolve_interactive(&opts)?
    };

    if opts.download_model && resolved.provider_name != "mlx" {
        anyhow::bail!("--download-model requires --provider mlx");
    }

    upsert_vision_platform(
        &config_path,
        LOOPBACK_ENDPOINT,
        VISION_TOKEN_ENV,
        &resolved.provider_name,
        &resolved.profile,
    )
    .map_err(|error| anyhow!("{error}"))?;

    println!(
        "Wrote vision provider settings and [nodes.vision] to {}",
        config_path.display()
    );
    print_env_hints(&resolved)?;

    if opts.activate {
        match crate::vision_readiness::check_provider_readiness(
            &resolved.provider_name,
            &resolved.profile,
            &crate::vision_readiness::ReadinessOptions {
                timeout: Duration::from_secs(45),
                allow_download: opts.download_model,
            },
        )? {
            crate::vision_readiness::ReadinessOutcome::Ready { provider, model } => {
                println!("Loaded and readiness-tested {provider} model {model}");
            }
            outcome @ crate::vision_readiness::ReadinessOutcome::NeedsAction { .. } => {
                anyhow::bail!("vision activation: {}", outcome.detail());
            }
        }
    }

    if !opts.yes {
        smoke_probe_upstream(&resolved.profile.base_url);
    }

    Ok(())
}

fn parse_auth(value: &str) -> Result<VisionAuthKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "advertised" => Ok(VisionAuthKind::Advertised),
        "oauth-authorization-code" => Ok(VisionAuthKind::OAuthAuthorizationCode),
        "oauth-device-code" => Ok(VisionAuthKind::OAuthDeviceCode),
        "environment" => Ok(VisionAuthKind::Environment),
        "existing-session" => Ok(VisionAuthKind::ExistingSession),
        "none" => Ok(VisionAuthKind::None),
        _ => anyhow::bail!("unsupported auth path {value:?}"),
    }
}

fn resolve_non_interactive(opts: &ConnectOpts) -> Result<ResolvedProfile> {
    let provider = opts
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("--yes requires --provider (openai|ollama|lmstudio|mlx|custom)"))?;

    if provider.eq_ignore_ascii_case("custom") {
        let base_url = opts
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("--yes --provider custom requires --base-url"))?;
        let model = opts
            .model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("--yes --provider custom requires --model"))?;
        let api_key_env = normalize_api_key_env(opts.api_key_env.as_deref());
        return Ok(ResolvedProfile {
            provider_name: "custom".to_owned(),
            profile: VisionProviderConfig {
                base_url: base_url.to_owned(),
                model: model.to_owned(),
                api_key_env,
            },
        });
    }

    if let Some(overrides) = preset_overrides(provider, opts)? {
        return Ok(overrides);
    }

    Err(anyhow!(
        "unknown provider {provider:?}; expected openai, ollama, lmstudio, mlx, or custom"
    ))
}

fn preset_overrides(provider: &str, opts: &ConnectOpts) -> Result<Option<ResolvedProfile>> {
    let Some((name, mut profile)) = preset(provider) else {
        return Ok(None);
    };
    if let Some(base_url) = opts
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        profile.base_url = base_url.to_owned();
    }
    if let Some(model) = opts
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        profile.model = model.to_owned();
    }
    if opts.api_key_env.is_some() {
        profile.api_key_env = normalize_api_key_env(opts.api_key_env.as_deref());
    }
    Ok(Some(ResolvedProfile {
        provider_name: name,
        profile,
    }))
}

fn resolve_interactive(opts: &ConnectOpts) -> Result<ResolvedProfile> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        anyhow::bail!(
            "bobby vision connect needs a terminal for prompts, or pass --yes with --provider"
        );
    }

    if let Some(provider) = opts
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if provider.eq_ignore_ascii_case("custom") {
            let base_url = match opts
                .base_url
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                Some(value) => value.to_owned(),
                None => prompt_line("Base URL", None)?,
            };
            let model = match opts
                .model
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                Some(value) => value.to_owned(),
                None => prompt_line("Model", None)?,
            };
            let api_key_env = if opts.api_key_env.is_some() {
                normalize_api_key_env(opts.api_key_env.as_deref())
            } else {
                normalize_api_key_env(
                    prompt_optional("API key env var (empty for none)")?.as_deref(),
                )
            };
            return Ok(ResolvedProfile {
                provider_name: prompt_line("Provider name", Some("custom"))?,
                profile: VisionProviderConfig {
                    base_url,
                    model,
                    api_key_env,
                },
            });
        }
        if let Some(resolved) = preset_overrides(provider, opts)? {
            return Ok(resolved);
        }
        anyhow::bail!(
            "unknown provider {provider:?}; expected openai, ollama, lmstudio, mlx, or custom"
        );
    }

    println!("Vision provider:");
    println!("  1) openai   — OpenAI cloud API");
    println!("  2) ollama   — local Ollama");
    println!("  3) lmstudio — local LM Studio");
    println!("  4) mlx      — local MLX vision (canonical server, Apple Silicon)");
    println!("  5) custom   — other OpenAI-compatible endpoint");
    let choice = prompt_line("Choice [1-5]", Some("4"))?;
    let resolved = match choice.trim() {
        "1" | "openai" => preset("openai")
            .map(|(name, profile)| ResolvedProfile {
                provider_name: name,
                profile,
            })
            .ok_or_else(|| anyhow!("openai preset unavailable"))?,
        "2" | "ollama" => preset("ollama")
            .map(|(name, profile)| ResolvedProfile {
                provider_name: name,
                profile,
            })
            .ok_or_else(|| anyhow!("ollama preset unavailable"))?,
        "3" | "lmstudio" => preset("lmstudio")
            .map(|(name, profile)| ResolvedProfile {
                provider_name: name,
                profile,
            })
            .ok_or_else(|| anyhow!("lmstudio preset unavailable"))?,
        "4" | "mlx" => preset("mlx")
            .map(|(name, profile)| ResolvedProfile {
                provider_name: name,
                profile,
            })
            .ok_or_else(|| anyhow!("mlx preset unavailable"))?,
        "5" | "custom" => ResolvedProfile {
            provider_name: prompt_line("Provider name", Some("custom"))?,
            profile: VisionProviderConfig {
                base_url: prompt_line("Base URL", None)?,
                model: prompt_line("Model", None)?,
                api_key_env: normalize_api_key_env(
                    prompt_optional("API key env var (empty for none)")?.as_deref(),
                ),
            },
        },
        other => return Err(anyhow!("invalid choice {other:?}")),
    };
    Ok(resolved)
}

pub(crate) fn preset(name: &str) -> Option<(String, VisionProviderConfig)> {
    let profile = match name {
        "openai" => VisionProviderConfig {
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o-mini".into(),
            api_key_env: Some("OPENAI_API_KEY".into()),
        },
        "ollama" => VisionProviderConfig {
            base_url: "http://127.0.0.1:11434/v1".into(),
            model: "llava".into(),
            api_key_env: None,
        },
        "lmstudio" => VisionProviderConfig {
            base_url: "http://127.0.0.1:1234/v1".into(),
            model: "local-model".into(),
            api_key_env: None,
        },
        "mlx" => VisionProviderConfig {
            base_url: "http://127.0.0.1:9101".into(),
            model: "mlx-community/Qwen2.5-VL-3B-Instruct-4bit".into(),
            api_key_env: None,
        },
        _ => return None,
    };
    Some((name.to_owned(), profile))
}

fn normalize_api_key_env(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

fn print_env_hints(resolved: &ResolvedProfile) -> Result<()> {
    eprintln!("Set the loopback bearer before `bobby serve --vision`:");
    eprintln!("  export {VISION_TOKEN_ENV}=…");
    if let Some(api_key_env) = &resolved.profile.api_key_env {
        eprintln!("Set the upstream API key env var:");
        eprintln!("  export {api_key_env}=…");
    }
    Ok(())
}

fn smoke_probe_upstream(base_url: &str) {
    let Ok(url) = url::Url::parse(base_url) else {
        eprintln!("warn: could not parse base URL for reachability check: {base_url}");
        return;
    };
    let host = match url.host_str() {
        Some(host) => host.to_owned(),
        None => {
            eprintln!("warn: base URL has no host for reachability check: {base_url}");
            return;
        }
    };
    let port = url.port_or_known_default().unwrap_or(80);
    let target = format!("{host}:{port}");
    let addrs: Vec<SocketAddr> = match target.to_socket_addrs() {
        Ok(addrs) => addrs.collect(),
        Err(error) => {
            eprintln!("warn: could not resolve {target} for reachability check: {error}");
            return;
        }
    };
    let reachable = addrs
        .iter()
        .any(|addr| std::net::TcpStream::connect_timeout(addr, Duration::from_millis(500)).is_ok());
    if reachable {
        eprintln!("ok: upstream reachable at {base_url}");
    } else {
        eprintln!("warn: upstream not reachable at {base_url} (is the server running?)");
    }
}

fn prompt_line(label: &str, default: Option<&str>) -> Result<String> {
    let mut stdout = std::io::stdout();
    match default {
        Some(default) => write!(stdout, "{label} [{default}]: ")?,
        None => write!(stdout, "{label}: ")?,
    }
    stdout.flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        default
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("{label} is required"))
    } else {
        Ok(trimmed.to_owned())
    }
}

fn prompt_optional(label: &str) -> Result<Option<String>> {
    let value = prompt_line(label, Some(""))?;
    Ok(normalize_api_key_env(Some(&value)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::AppConfig;

    #[test]
    fn connect_yes_lmstudio_writes_profile() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        connect(ConnectOpts {
            config: Some(config_path.clone()),
            provider: Some("lmstudio".into()),
            yes: true,
            ..ConnectOpts::default()
        })
        .unwrap();

        let loaded = AppConfig::load(&config_path).unwrap();
        assert_eq!(loaded.vision.provider.as_deref(), Some("lmstudio"));
        // The endpoint lives in [nodes.vision]; `upsert_vision_platform`
        // drops the legacy [vision] copies so there is one source of truth.
        let node = loaded.nodes.get("vision").expect("[nodes.vision] written");
        assert_eq!(node.endpoint_url, LOOPBACK_ENDPOINT);
        assert_eq!(node.token_env.as_deref(), Some(VISION_TOKEN_ENV));
        assert!(loaded.vision.endpoint_url.is_none());
        assert!(loaded.vision.token_env.is_none());
        let (_, profile) = loaded.vision.selected_provider().unwrap();
        assert_eq!(profile.base_url, "http://127.0.0.1:1234/v1");
        assert_eq!(profile.model, "local-model");
        assert!(profile.api_key_env.is_none());

        let text = std::fs::read_to_string(&config_path).unwrap();
        assert!(!text.contains("sk-"));
        assert!(!text.contains("api_key ="));
    }

    #[test]
    fn connect_yes_acp_writes_no_secret() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        connect(ConnectOpts {
            config: Some(config_path.clone()),
            backend: "acp".into(),
            provider: Some("codex".into()),
            command: Some("codex".into()),
            args: vec!["acp".into()],
            auth: "advertised".into(),
            yes: true,
            ..ConnectOpts::default()
        })
        .unwrap();
        let loaded = AppConfig::load(&config_path).unwrap();
        assert!(matches!(
            loaded.vision.selected_backend(),
            Some(config::VisionBackendSelection::Acp { name: "codex", .. })
        ));
        let text = std::fs::read_to_string(config_path).unwrap();
        assert!(!text.contains("token"));
        assert!(!text.contains("secret"));
    }

    #[test]
    fn connect_yes_openai_writes_profile_without_secret() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        connect(ConnectOpts {
            config: Some(config_path.clone()),
            provider: Some("openai".into()),
            yes: true,
            ..ConnectOpts::default()
        })
        .unwrap();

        let loaded = AppConfig::load(&config_path).unwrap();
        assert_eq!(loaded.vision.provider.as_deref(), Some("openai"));
        let (_, profile) = loaded.vision.selected_provider().unwrap();
        assert_eq!(profile.base_url, "https://api.openai.com/v1");
        assert_eq!(profile.model, "gpt-4o-mini");
        assert_eq!(profile.api_key_env.as_deref(), Some("OPENAI_API_KEY"));

        let text = std::fs::read_to_string(&config_path).unwrap();
        assert!(text.contains("api_key_env = \"OPENAI_API_KEY\""));
        assert!(!text.contains("sk-"));
    }

    #[test]
    fn connect_yes_custom_requires_base_url_and_model() {
        let error = connect(ConnectOpts {
            provider: Some("custom".into()),
            yes: true,
            ..ConnectOpts::default()
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("--base-url"));
    }

    #[test]
    fn preset_values_match_spec() {
        let (_, openai) = preset("openai").unwrap();
        assert_eq!(openai.base_url, "https://api.openai.com/v1");
        assert_eq!(openai.model, "gpt-4o-mini");
        assert_eq!(openai.api_key_env.as_deref(), Some("OPENAI_API_KEY"));

        let (_, ollama) = preset("ollama").unwrap();
        assert_eq!(ollama.base_url, "http://127.0.0.1:11434/v1");
        assert_eq!(ollama.model, "llava");
        assert!(ollama.api_key_env.is_none());

        let (_, lmstudio) = preset("lmstudio").unwrap();
        assert_eq!(lmstudio.base_url, "http://127.0.0.1:1234/v1");
        assert_eq!(lmstudio.model, "local-model");
        assert!(lmstudio.api_key_env.is_none());
    }
}
