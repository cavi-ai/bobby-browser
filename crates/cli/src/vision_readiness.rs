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
    pub(crate) allow_start: bool,
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

    let mut probe =
        openai_compatible_models_probe(&provider, profile, api_key.as_deref(), options.timeout);
    if matches!(probe, ProbeResult::Unreachable)
        && options.allow_start
        && provider == "ollama"
        && try_start_ollama(profile, options.timeout)
    {
        probe =
            openai_compatible_models_probe(&provider, profile, api_key.as_deref(), options.timeout);
    }

    match probe {
        ProbeResult::Reachable { models } => {
            if provider == "ollama" {
                if let Some(models) = models {
                    if !ollama_model_listed(&profile.model, &models) {
                        if options.allow_download && pull_ollama_model(&profile.model) {
                            return Ok(ReadinessOutcome::Ready {
                                provider,
                                model: profile.model.clone(),
                            });
                        }
                        return Ok(ReadinessOutcome::NeedsAction {
                            provider,
                            model: profile.model.clone(),
                            detail: format!(
                                "Ollama is running at {} but model {} is not installed; run `ollama pull {}`",
                                profile.base_url, profile.model, profile.model
                            ),
                        });
                    }
                }
            }
            Ok(ReadinessOutcome::Ready {
                provider,
                model: profile.model.clone(),
            })
        }
        ProbeResult::Unreachable => Ok(ReadinessOutcome::NeedsAction {
            provider: provider.clone(),
            model: profile.model.clone(),
            detail: match provider.as_str() {
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
            },
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeResult {
    Unreachable,
    Reachable { models: Option<Vec<String>> },
}

pub(crate) fn models_probe_urls(provider: &str, base_url: &str) -> Vec<String> {
    let base = base_url.trim_end_matches('/');
    if provider.eq_ignore_ascii_case("ollama") && !base.ends_with("/v1") {
        vec![format!("{base}/v1/models"), format!("{base}/api/tags")]
    } else {
        vec![format!("{base}/models")]
    }
}

pub(crate) fn ollama_model_listed(configured: &str, listed: &[String]) -> bool {
    let configured = configured.trim();
    if configured.is_empty() {
        return false;
    }
    listed.iter().any(|id| {
        id == configured
            || id.starts_with(&format!("{configured}:"))
            || configured.starts_with(&format!("{id}:"))
    })
}

fn listed_models(body: &str) -> Option<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    if let Some(data) = value.get("data").and_then(|value| value.as_array()) {
        return Some(
            data.iter()
                .filter_map(|model| {
                    model
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .collect(),
        );
    }
    if let Some(models) = value.get("models").and_then(|value| value.as_array()) {
        return Some(
            models
                .iter()
                .filter_map(|model| {
                    model
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .collect(),
        );
    }
    None
}

fn openai_compatible_models_probe(
    provider: &str,
    profile: &VisionProviderConfig,
    api_key: Option<&str>,
    timeout: Duration,
) -> ProbeResult {
    let urls = models_probe_urls(provider, &profile.base_url);
    let api_key = api_key.map(str::to_owned);
    std::thread::spawn(move || {
        let Ok(client) = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .no_proxy()
            .build()
        else {
            return ProbeResult::Unreachable;
        };
        for url in urls {
            let mut request = client.get(&url);
            if let Some(api_key) = api_key.as_deref() {
                request = request.bearer_auth(api_key);
            }
            let Ok(response) = request.send() else {
                continue;
            };
            if !response.status().is_success() {
                continue;
            }
            let body = response.text().unwrap_or_default();
            return ProbeResult::Reachable {
                models: listed_models(&body),
            };
        }
        ProbeResult::Unreachable
    })
    .join()
    .unwrap_or(ProbeResult::Unreachable)
}

fn try_start_ollama(profile: &VisionProviderConfig, timeout: Duration) -> bool {
    let Some(address) = endpoint_socket(&profile.base_url) else {
        return false;
    };
    if !address.ip().is_loopback() {
        return false;
    }
    if TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_ok() {
        return true;
    }
    let Some(binary) = crate::onboarding::find_sidecar_binary("ollama") else {
        return false;
    };
    let Ok(mut child) = Command::new(binary)
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let _ = child.try_wait();
        if TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn pull_ollama_model(model: &str) -> bool {
    let Some(binary) = crate::onboarding::find_sidecar_binary("ollama") else {
        return false;
    };
    Command::new(binary)
        .arg("pull")
        .arg(model)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .ok()
        .is_some_and(|status| status.success())
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
            allow_start: false,
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

    #[tokio::test]
    async fn external_readiness_is_safe_inside_the_cli_async_runtime() {
        let profile = VisionProviderConfig {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "llava".into(),
            api_key_env: None,
        };
        let outcome = check_provider_readiness(
            "ollama",
            &profile,
            &ReadinessOptions {
                timeout: Duration::from_millis(10),
                allow_download: false,
                allow_start: false,
            },
        )
        .unwrap();
        assert!(matches!(outcome, ReadinessOutcome::NeedsAction { .. }));
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

    #[test]
    fn ollama_host_only_base_probes_openai_v1_models() {
        assert_eq!(
            models_probe_urls("ollama", "http://127.0.0.1:11434"),
            vec![
                "http://127.0.0.1:11434/v1/models".to_string(),
                "http://127.0.0.1:11434/api/tags".to_string(),
            ]
        );
        assert_eq!(
            models_probe_urls("ollama", "http://127.0.0.1:11434/v1"),
            vec!["http://127.0.0.1:11434/v1/models".to_string()]
        );
        assert_eq!(
            models_probe_urls("openai", "https://api.openai.com/v1"),
            vec!["https://api.openai.com/v1/models".to_string()]
        );
    }

    #[test]
    fn ollama_model_listed_accepts_tagged_variants() {
        let listed = ["llava:7b".to_string(), "qwen3.8:27b-mlx".to_string()];
        assert!(ollama_model_listed("llava", &listed));
        assert!(ollama_model_listed("llava:7b", &listed));
        assert!(!ollama_model_listed("llava:13b", &listed));
        assert!(!ollama_model_listed("llama3.1", &listed));
    }

    #[test]
    fn ollama_host_only_readiness_succeeds_against_v1_models() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0_u8; 256];
                let read = stream.read(&mut chunk).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            let request = String::from_utf8_lossy(&request);
            assert!(
                request.contains("GET /v1/models"),
                "host-only ollama base must probe /v1/models, got {request}"
            );
            let body = r#"{"object":"list","data":[{"id":"llava:7b"}]}"#;
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        });

        let profile = VisionProviderConfig {
            base_url: format!("http://{address}"),
            model: "llava".into(),
            api_key_env: None,
        };
        let outcome = check_provider_readiness(
            "ollama",
            &profile,
            &ReadinessOptions {
                timeout: Duration::from_secs(2),
                allow_download: false,
                allow_start: false,
            },
        )
        .unwrap();
        assert!(
            matches!(
                outcome,
                ReadinessOutcome::Ready { ref model, .. } if model == "llava"
            ),
            "{outcome:?}"
        );
        server.join().unwrap();
    }
}
