use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use config::{AppConfig, NodeConfig, NodeKind, VisionProviderConfig};
use node_registry::{NodeRegistry, LEGACY_VISION_NODE};
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisionSpawnPolicy {
    Off,
    Auto,
    ForceOn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisionChildDecision {
    pub should_spawn: bool,
    pub bind: SocketAddr,
    pub path: String,
    pub reason: String,
}

const DEFAULT_BIND: SocketAddr = SocketAddr::new(
    std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    9100,
);

impl VisionChildDecision {
    fn skipped(reason: impl Into<String>) -> Self {
        Self {
            should_spawn: false,
            bind: DEFAULT_BIND,
            path: "/vision".to_string(),
            reason: reason.into(),
        }
    }
}

pub fn decide_vision_child(config: &AppConfig, policy: VisionSpawnPolicy) -> VisionChildDecision {
    decide_vision_child_with_probe(config, policy, is_port_accepting)
}

fn decide_vision_child_with_probe(
    config: &AppConfig,
    policy: VisionSpawnPolicy,
    probe: fn(SocketAddr) -> bool,
) -> VisionChildDecision {
    if matches!(policy, VisionSpawnPolicy::Off) {
        return VisionChildDecision::skipped("vision spawn policy is off");
    }

    let registry = NodeRegistry::from_config(config);
    if registry.is_empty() {
        return VisionChildDecision::skipped(match policy {
            VisionSpawnPolicy::ForceOn => "no vision node configured; mint required before spawn",
            _ => "no vision node configured",
        });
    }

    let Some(node_name) = select_vision_node_name(&registry, policy) else {
        return VisionChildDecision::skipped(
            "multiple vision nodes configured; refusing to spawn",
        );
    };

    let Some(node) = node_config(config, &node_name) else {
        return VisionChildDecision::skipped(format!(
            "vision node {node_name} not found in config"
        ));
    };

    if !node.is_local() {
        return VisionChildDecision::skipped(format!(
            "vision node {node_name} is not loopback-local"
        ));
    }

    let Some((bind, path)) = parse_loopback_endpoint(&node.endpoint_url) else {
        return VisionChildDecision::skipped(format!(
            "vision node {node_name} endpoint URL is invalid"
        ));
    };

    if config.vision.selected_provider().is_none() {
        return VisionChildDecision {
            should_spawn: false,
            bind,
            path,
            reason: match policy {
                VisionSpawnPolicy::ForceOn => {
                    "no vision provider selected; connect or configure a provider".to_string()
                }
                _ => "no vision provider selected".to_string(),
            },
        };
    }

    if probe(bind) {
        return VisionChildDecision {
            should_spawn: false,
            bind,
            path,
            reason: "vision endpoint already reachable".to_string(),
        };
    }

    VisionChildDecision {
        should_spawn: true,
        bind,
        path,
        reason: "loopback vision endpoint not reachable; spawn required".to_string(),
    }
}

fn select_vision_node_name(
    registry: &NodeRegistry,
    policy: VisionSpawnPolicy,
) -> Option<String> {
    if registry.resolve(LEGACY_VISION_NODE, NodeKind::Vision).is_ok() {
        return Some(LEGACY_VISION_NODE.to_string());
    }

    let vision_names: Vec<String> = registry
        .names()
        .filter(|name| registry.resolve(name, NodeKind::Vision).is_ok())
        .map(str::to_owned)
        .collect();

    match vision_names.len() {
        0 => None,
        1 => Some(vision_names[0].clone()),
        _ => match policy {
            VisionSpawnPolicy::ForceOn => registry
                .resolve(LEGACY_VISION_NODE, NodeKind::Vision)
                .ok()
                .map(|_| LEGACY_VISION_NODE.to_string()),
            _ => None,
        },
    }
}

fn node_config(config: &AppConfig, name: &str) -> Option<NodeConfig> {
    if let Some(node) = config.nodes.get(name) {
        return Some(node.clone());
    }
    if name == LEGACY_VISION_NODE && config.nodes.is_empty() {
        let endpoint_url = config.vision.endpoint_url.clone()?;
        Some(NodeConfig {
            kind: NodeKind::Vision,
            endpoint_url,
            token_env: config.vision.token_env.clone(),
            timeout_ms: config.vision.timeout_ms,
        })
    } else {
        None
    }
}

fn parse_loopback_endpoint(endpoint_url: &str) -> Option<(SocketAddr, String)> {
    let url = Url::parse(endpoint_url).ok()?;
    let host = url.host_str()?;
    let port = url.port_or_known_default()?;
    let bind: SocketAddr = format!("{host}:{port}").parse().ok()?;
    let path = if url.path().is_empty() {
        "/".to_string()
    } else {
        url.path().to_string()
    };
    Some((bind, path))
}

fn is_port_accepting(addr: SocketAddr) -> bool {
    TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
}

pub struct ManagedVisionProxy {
    child: Child,
}

impl ManagedVisionProxy {
    pub fn spawn_from_current_exe(
        decision: &VisionChildDecision,
        profile: &VisionProviderConfig,
        token_env: &str,
    ) -> Result<Self> {
        if !decision.should_spawn {
            anyhow::bail!("vision child spawn not requested: {}", decision.reason);
        }

        if std::env::var(token_env)
            .ok()
            .filter(|value| !value.is_empty())
            .is_none()
        {
            anyhow::bail!("{token_env} must be set before spawning vision-proxy");
        }

        let exe = std::env::current_exe().context("failed to resolve current executable")?;
        let mut cmd = Command::new(exe);
        cmd.arg("vision-proxy")
            .arg("--bind")
            .arg(decision.bind.to_string())
            .arg("--path")
            .arg(&decision.path)
            .arg("--model")
            .arg(&profile.model)
            .arg("--openai-base-url")
            .arg(&profile.base_url);
        match &profile.api_key_env {
            Some(name) => {
                cmd.arg("--api-key-env").arg(name);
            }
            None => {
                cmd.arg("--api-key-env").arg("");
            }
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().context("failed to spawn vision-proxy child")?;

        std::thread::sleep(Duration::from_millis(250));
        if !is_port_accepting(decision.bind) {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "vision-proxy did not become reachable on {}",
                decision.bind
            );
        }

        Ok(Self { child })
    }
}

impl Drop for ManagedVisionProxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::{VisionConfig, VisionProviderConfig};
    use std::collections::BTreeMap;

    fn sample_provider() -> VisionProviderConfig {
        VisionProviderConfig {
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o".into(),
            api_key_env: Some("OPENAI_API_KEY".into()),
        }
    }

    fn loopback_config(endpoint: &str, with_provider: bool) -> AppConfig {
        let config = AppConfig {
            vision: VisionConfig {
                endpoint_url: Some(endpoint.into()),
                token_env: Some("BOBBY_VISION_TOKEN".into()),
                timeout_ms: 15_000,
                provider: if with_provider {
                    Some("openai".into())
                } else {
                    None
                },
                providers: if with_provider {
                    BTreeMap::from([("openai".into(), sample_provider())])
                } else {
                    BTreeMap::new()
                },
            },
            ..AppConfig::default()
        };
        config
    }

    #[test]
    fn no_vision_policy_never_spawns() {
        let config = loopback_config("http://127.0.0.1:19876/vision", true);
        let decision =
            decide_vision_child_with_probe(&config, VisionSpawnPolicy::Off, |_| false);
        assert!(!decision.should_spawn);
        assert!(decision.reason.contains("off"));
    }

    #[test]
    fn non_loopback_never_spawns() {
        let config = loopback_config("https://vision.example/vision", true);
        let decision =
            decide_vision_child_with_probe(&config, VisionSpawnPolicy::Auto, |_| false);
        assert!(!decision.should_spawn);
        assert!(decision.reason.contains("loopback"));
    }

    #[test]
    fn loopback_with_provider_auto_spawns() {
        let config = loopback_config("http://127.0.0.1:19876/vision", true);
        let decision =
            decide_vision_child_with_probe(&config, VisionSpawnPolicy::Auto, |_| false);
        assert!(decision.should_spawn);
        assert_eq!(decision.bind, "127.0.0.1:19876".parse().unwrap());
        assert_eq!(decision.path, "/vision");
    }

    #[test]
    fn loopback_without_provider_auto_skips() {
        let config = loopback_config("http://127.0.0.1:19876/vision", false);
        let decision =
            decide_vision_child_with_probe(&config, VisionSpawnPolicy::Auto, |_| false);
        assert!(!decision.should_spawn);
        assert!(decision.reason.contains("provider"));
    }

    #[test]
    fn loopback_reachable_port_skips_spawn() {
        let config = loopback_config("http://127.0.0.1:19876/vision", true);
        let decision =
            decide_vision_child_with_probe(&config, VisionSpawnPolicy::Auto, |_| true);
        assert!(!decision.should_spawn);
        assert!(decision.reason.contains("reachable"));
    }

    #[test]
    fn empty_registry_auto_skips() {
        let config = AppConfig::default();
        let decision =
            decide_vision_child_with_probe(&config, VisionSpawnPolicy::Auto, |_| false);
        assert!(!decision.should_spawn);
        assert!(decision.reason.contains("no vision node"));
    }

    #[test]
    fn multiple_vision_nodes_auto_skips() {
        let mut config = AppConfig::default();
        config.nodes.insert(
            "alpha".into(),
            NodeConfig {
                kind: NodeKind::Vision,
                endpoint_url: "http://127.0.0.1:19876/vision".into(),
                token_env: None,
                timeout_ms: 15_000,
            },
        );
        config.nodes.insert(
            "beta".into(),
            NodeConfig {
                kind: NodeKind::Vision,
                endpoint_url: "http://127.0.0.1:19877/vision".into(),
                token_env: None,
                timeout_ms: 15_000,
            },
        );
        config.vision.provider = Some("openai".into());
        config
            .vision
            .providers
            .insert("openai".into(), sample_provider());

        let decision =
            decide_vision_child_with_probe(&config, VisionSpawnPolicy::Auto, |_| false);
        assert!(!decision.should_spawn);
        assert!(decision.reason.contains("multiple"));
    }

    #[test]
    fn prefers_legacy_vision_node_name() {
        let mut config = AppConfig::default();
        config.nodes.insert(
            "vision".into(),
            NodeConfig {
                kind: NodeKind::Vision,
                endpoint_url: "http://127.0.0.1:19878/vision".into(),
                token_env: None,
                timeout_ms: 15_000,
            },
        );
        config.nodes.insert(
            "other".into(),
            NodeConfig {
                kind: NodeKind::Vision,
                endpoint_url: "http://127.0.0.1:19879/vision".into(),
                token_env: None,
                timeout_ms: 15_000,
            },
        );
        config.vision.provider = Some("openai".into());
        config
            .vision
            .providers
            .insert("openai".into(), sample_provider());

        let decision =
            decide_vision_child_with_probe(&config, VisionSpawnPolicy::Auto, |_| false);
        assert!(decision.should_spawn);
        assert_eq!(decision.bind, "127.0.0.1:19878".parse().unwrap());
    }
}
