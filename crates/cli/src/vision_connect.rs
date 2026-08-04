//! `bobby vision connect` — configure a named vision provider profile in config.toml.

use std::io::{BufRead, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Result};
use config::{upsert_vision_platform, VisionProviderConfig};

pub const LOOPBACK_ENDPOINT: &str = "http://127.0.0.1:9100/vision";
pub const VISION_TOKEN_ENV: &str = "BOBBY_VISION_TOKEN";

/// CLI flags for `bobby vision connect`.
#[derive(Debug, Clone, Default)]
pub struct ConnectOpts {
    pub config: Option<PathBuf>,
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key_env: Option<String>,
    pub yes: bool,
}

struct ResolvedProfile {
    provider_name: String,
    profile: VisionProviderConfig,
}

/// Configure vision provider settings and persist them to config.toml.
pub fn connect(opts: ConnectOpts) -> Result<()> {
    let config_path = crate::resolve_config_path(opts.config.clone());
    let resolved = if opts.yes {
        resolve_non_interactive(&opts)?
    } else {
        resolve_interactive(&opts)?
    };

    upsert_vision_platform(
        &config_path,
        LOOPBACK_ENDPOINT,
        VISION_TOKEN_ENV,
        &resolved.provider_name,
        &resolved.profile,
    )
    .map_err(|error| anyhow!("{error}"))?;

    println!("Wrote vision settings to {}", config_path.display());
    print_env_hints(&resolved)?;

    if !opts.yes {
        smoke_probe_upstream(&resolved.profile.base_url);
    }

    Ok(())
}

fn resolve_non_interactive(opts: &ConnectOpts) -> Result<ResolvedProfile> {
    let provider = opts
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!("--yes requires --provider (openai|ollama|lmstudio|custom)")
        })?;

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
        "unknown provider {provider:?}; expected openai, ollama, lmstudio, or custom"
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
                    prompt_optional("API key env var (empty for none)")?
                        .as_deref(),
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
            "unknown provider {provider:?}; expected openai, ollama, lmstudio, or custom"
        );
    }

    println!("Vision provider:");
    println!("  1) openai   — OpenAI cloud API");
    println!("  2) ollama   — local Ollama");
    println!("  3) lmstudio — local LM Studio");
    println!("  4) custom   — other OpenAI-compatible endpoint");
    let choice = prompt_line("Choice [1-4]", Some("3"))?;
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
        "4" | "custom" => ResolvedProfile {
            provider_name: prompt_line("Provider name", Some("custom"))?,
            profile: VisionProviderConfig {
                base_url: prompt_line("Base URL", None)?,
                model: prompt_line("Model", None)?,
                api_key_env: normalize_api_key_env(
                    prompt_optional("API key env var (empty for none)")?
                        .as_deref(),
                ),
            },
        },
        other => return Err(anyhow!("invalid choice {other:?}")),
    };
    Ok(resolved)
}

fn preset(name: &str) -> Option<(String, VisionProviderConfig)> {
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
    let reachable = addrs.iter().any(|addr| {
        std::net::TcpStream::connect_timeout(addr, Duration::from_millis(500)).is_ok()
    });
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
        assert_eq!(
            loaded.vision.endpoint_url.as_deref(),
            Some(LOOPBACK_ENDPOINT)
        );
        assert_eq!(loaded.vision.token_env.as_deref(), Some(VISION_TOKEN_ENV));
        let (_, profile) = loaded.vision.selected_provider().unwrap();
        assert_eq!(profile.base_url, "http://127.0.0.1:1234/v1");
        assert_eq!(profile.model, "local-model");
        assert!(profile.api_key_env.is_none());

        let text = std::fs::read_to_string(&config_path).unwrap();
        assert!(!text.contains("sk-"));
        assert!(!text.contains("api_key ="));
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
