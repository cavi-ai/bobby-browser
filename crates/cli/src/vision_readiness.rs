use std::{
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use config::VisionProviderConfig;

#[derive(Debug, Clone)]
pub(crate) struct ReadinessOptions {
    pub(crate) timeout: Duration,
    pub(crate) allow_download: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReadinessOutcome {
    Ready {
        provider: String,
        model: String,
    },
    NeedsAction {
        provider: String,
        model: String,
        detail: String,
    },
}

impl ReadinessOutcome {
    pub(crate) fn detail(&self) -> &str {
        match self {
            Self::Ready { .. } => "ready",
            Self::NeedsAction { detail, .. } => detail,
        }
    }
}

fn hugging_face_cache_root() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("HF_HOME") {
        return Ok(PathBuf::from(root).join("hub"));
    }
    Ok(dirs::home_dir()
        .context("home directory unavailable")?
        .join(".cache/huggingface/hub"))
}

pub(crate) fn cached_hugging_face_model(model: &str) -> Result<bool> {
    cached_hugging_face_model_at(
        hugging_face_cache_root()?
            .parent()
            .context("Hugging Face cache root has no parent")?,
        model,
    )
}

fn cached_hugging_face_model_at(hf_home: &Path, model: &str) -> Result<bool> {
    let snapshots = hf_home
        .join("hub")
        .join(format!("models--{}", model.replace('/', "--")))
        .join("snapshots");
    let Ok(entries) = std::fs::read_dir(snapshots) else {
        return Ok(false);
    };
    Ok(entries.flatten().any(|entry| {
        let snapshot = entry.path();
        snapshot.join("config.json").is_file()
            && snapshot.join("preprocessor_config.json").is_file()
    }))
}

fn hugging_face_download_command(model: &str) -> Result<Command> {
    for binary in ["hf", "huggingface-cli"] {
        if let Some(path) = crate::onboarding::find_sidecar_binary(binary) {
            let mut command = Command::new(path);
            command.arg("download").arg(model);
            return Ok(command);
        }
    }
    anyhow::bail!("Hugging Face downloader not found; install huggingface_hub so hf is on PATH")
}

pub(crate) fn download_and_verify_mlx_model(model: &str) -> Result<()> {
    let status = hugging_face_download_command(model)?
        .status()
        .with_context(|| format!("failed to download {model}"))?;
    if !status.success() {
        anyhow::bail!("Hugging Face download failed for {model} with {status}");
    }
    if !cached_hugging_face_model(model)? {
        anyhow::bail!("download finished but no complete cached snapshot was found for {model}");
    }
    Ok(())
}

pub(crate) fn check_provider_readiness(
    provider_name: &str,
    profile: &VisionProviderConfig,
    options: &ReadinessOptions,
) -> Result<ReadinessOutcome> {
    let provider = provider_name.trim().to_ascii_lowercase();
    if provider == "mlx" {
        if !cached_hugging_face_model(&profile.model)? {
            if options.allow_download {
                download_and_verify_mlx_model(&profile.model)?;
            } else {
                return Ok(ReadinessOutcome::NeedsAction {
                    provider,
                    model: profile.model.clone(),
                    detail: format!(
                        "selected MLX model {} is not cached; run `bobby doctor --fix --download-model`",
                        profile.model
                    ),
                });
            }
        }
        if endpoint_socket(&profile.base_url)
            .is_some_and(|address| TcpStream::connect_timeout(&address, options.timeout).is_ok())
        {
            return Ok(ReadinessOutcome::Ready {
                provider,
                model: profile.model.clone(),
            });
        }
        return check_mlx_readiness(profile, options.timeout);
    }

    let api_key = if let Some(name) = profile
        .api_key_env
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        match std::env::var(name).ok().filter(|value| !value.is_empty()) {
            Some(value) => Some(value),
            None => {
                return Ok(ReadinessOutcome::NeedsAction {
                    provider,
                    model: profile.model.clone(),
                    detail: format!(
                        "{name} is missing or empty; set it before loading model {}",
                        profile.model
                    ),
                });
            }
        }
    } else {
        None
    };

    if openai_compatible_models_probe(profile, api_key.as_deref(), options.timeout) {
        return Ok(ReadinessOutcome::Ready {
            provider,
            model: profile.model.clone(),
        });
    }

    let detail = match provider.as_str() {
        "ollama" => format!(
            "Ollama is not reachable at {}; run `ollama serve` and ensure model {} is installed",
            profile.base_url, profile.model
        ),
        "lmstudio" => format!(
            "LM Studio is not reachable at {}; load {} and start the local server in LM Studio",
            profile.base_url, profile.model
        ),
        "mlx" => format!(
            "MLX model {} is cached but not loaded; Bobby must start its managed worker",
            profile.model
        ),
        _ => format!(
            "provider {provider} is not reachable at {} for model {}",
            profile.base_url, profile.model
        ),
    };
    Ok(ReadinessOutcome::NeedsAction {
        provider,
        model: profile.model.clone(),
        detail,
    })
}

fn openai_compatible_models_probe(
    profile: &VisionProviderConfig,
    api_key: Option<&str>,
    timeout: Duration,
) -> bool {
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
    else {
        return false;
    };
    let mut request = client.get(format!("{}/models", profile.base_url.trim_end_matches('/')));
    if let Some(api_key) = api_key {
        request = request.bearer_auth(api_key);
    }
    request
        .send()
        .is_ok_and(|response| response.status().is_success())
}

fn configure_mlx_readiness_command(
    command: &mut Command,
    bind: SocketAddr,
    profile: &VisionProviderConfig,
) {
    let decision = crate::vision_child::VisionChildDecision {
        should_spawn: true,
        bind,
        path: "/vision".to_string(),
        reason: "setup readiness check".to_string(),
    };
    crate::vision_child::configure_vision_proxy_command(
        command,
        &decision,
        "mlx",
        profile,
        false,
        Path::new("data/vision"),
    );
}

fn check_mlx_readiness(
    profile: &VisionProviderConfig,
    timeout: Duration,
) -> Result<ReadinessOutcome> {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .context("failed to reserve readiness probe port")?;
    let bind = listener.local_addr()?;
    drop(listener);

    let mut command = Command::new(std::env::current_exe().context("current executable unknown")?);
    configure_mlx_readiness_command(&mut command, bind, profile);
    command
        .env(
            "BOBBY_VISION_TOKEN",
            format!("bobby-readiness-{}", uuid::Uuid::new_v4().simple()),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    let child = command
        .spawn()
        .context("failed to start MLX readiness worker")?;
    let mut child = ChildGuard(Some(child));
    let deadline = Instant::now() + timeout;
    loop {
        if TcpStream::connect_timeout(&bind, Duration::from_millis(200)).is_ok() {
            return Ok(ReadinessOutcome::Ready {
                provider: "mlx".to_string(),
                model: profile.model.clone(),
            });
        }
        if let Some(status) = child
            .0
            .as_mut()
            .context("MLX readiness child missing")?
            .try_wait()?
        {
            anyhow::bail!(
                "MLX readiness worker exited before loading {}: {status}",
                profile.model
            );
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "MLX model {} did not become ready at {} within {} second(s)",
                profile.model,
                profile.base_url,
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct ChildGuard(Option<Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn endpoint_socket(base_url: &str) -> Option<SocketAddr> {
    let url = url::Url::parse(base_url).ok()?;
    let port = url.port_or_known_default()?;
    match url.host()? {
        url::Host::Ipv4(ip) => Some(SocketAddr::new(ip.into(), port)),
        url::Host::Ipv6(ip) => Some(SocketAddr::new(ip.into(), port)),
        url::Host::Domain(name) if name.eq_ignore_ascii_case("localhost") => {
            Some(SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), port))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::VisionProviderConfig;
    use std::time::Duration;

    #[test]
    fn vision_readiness_cache_requires_model_and_preprocessor_configs() {
        let root = tempfile::tempdir().unwrap();
        let model = "mlx-community/example-selected";
        let snapshot = root
            .path()
            .join("hub/models--mlx-community--example-selected/snapshots/revision");
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::write(snapshot.join("config.json"), "{}").unwrap();

        assert!(!cached_hugging_face_model_at(root.path(), model).unwrap());

        std::fs::write(snapshot.join("preprocessor_config.json"), "{}").unwrap();
        assert!(cached_hugging_face_model_at(root.path(), model).unwrap());
    }

    #[test]
    fn vision_readiness_external_provider_names_provider_specific_action() {
        let ollama = VisionProviderConfig {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "llava".into(),
            api_key_env: None,
        };
        let lmstudio = VisionProviderConfig {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "local-model".into(),
            api_key_env: None,
        };
        let options = ReadinessOptions {
            timeout: Duration::from_millis(10),
            allow_download: false,
        };

        let ollama = check_provider_readiness("ollama", &ollama, &options).unwrap();
        let lmstudio = check_provider_readiness("lmstudio", &lmstudio, &options).unwrap();

        assert!(ollama.detail().contains("ollama serve"));
        assert!(lmstudio.detail().contains("load local-model"));
        assert!(lmstudio.detail().contains("LM Studio"));

        let openai = VisionProviderConfig {
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o-mini".into(),
            api_key_env: Some("BOBBY_TEST_MISSING_OPENAI_KEY".into()),
        };
        let openai = check_provider_readiness("openai", &openai, &options).unwrap();
        assert!(openai.detail().contains("BOBBY_TEST_MISSING_OPENAI_KEY"));
    }

    #[test]
    fn vision_readiness_mlx_command_loads_the_exact_selected_model() {
        let profile = VisionProviderConfig {
            base_url: "http://127.0.0.1:19101".into(),
            model: "mlx-community/example-selected".into(),
            api_key_env: None,
        };
        let mut command = std::process::Command::new("bobby");

        configure_mlx_readiness_command(&mut command, "127.0.0.1:19100".parse().unwrap(), &profile);
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert!(args.windows(2).any(|pair| pair == ["--upstream", "mlx"]));
        assert!(args.contains(&"--spawn-server".to_string()));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--model", "mlx-community/example-selected"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--vision-base-url", "http://127.0.0.1:19101"]));
    }
}
