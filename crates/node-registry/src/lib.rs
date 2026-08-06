//! Named, separately addressable nodes, selected per session.
//!
//! A session names a node; the registry resolves the name against
//! configuration, or declines. Two properties this must preserve:
//!
//! - **Local-first, and provable.** [`ResolvedNode::is_local`] is derived from
//!   the node's address, never from trusting its operator, so a session bound
//!   to a loopback node is checkably confined to this machine.
//! - **No silent fallback.** An unconfigured name, or one configured with the
//!   wrong kind, resolves to an error: never another node, never a remote
//!   default. Falling back would silently revoke a session's privacy property.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use auth_broker::AuthStrategy;
use config::{AppConfig, NodeConfig, NodeKind, VisionAcpProfile, VisionAuthKind};
use intent_engine::{HttpVisionAssist, VisionAssist};
use types::{CommandError, ErrorCode, ErrorLayer};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NodeError {
    #[error("no node named {0} is configured")]
    Unknown(String),
    /// Unreachable while `NodeKind` has a single variant. Keep it, and the
    /// check in `resolve`: without it a second kind would silently be handed
    /// out as another.
    #[error("node {name} is a {configured:?} node, not {requested:?}")]
    WrongKind {
        name: String,
        configured: NodeKind,
        requested: NodeKind,
    },
    #[error("node {name} could not be reached: {reason}")]
    Unreachable { name: String, reason: String },
}

impl NodeError {
    /// The declined-escalation error this fault produces.
    ///
    /// Deliberately not a `From<NodeError> for CommandError` impl: another
    /// `From` for `CommandError` makes `?` ambiguous in crates that already
    /// convert several error types into it. Every variant is a configuration
    /// or addressing fault that no retry fixes.
    pub fn into_command_error(self) -> CommandError {
        CommandError {
            code: ErrorCode::VisionAssistFailed,
            message: self.to_string(),
            layer: ErrorLayer::Workflow,
            retryable: false,
        }
    }
}

/// A node resolved from configuration, with its locality already determined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedNode {
    pub name: String,
    pub kind: NodeKind,
    is_local: bool,
}

impl ResolvedNode {
    /// Whether page material sent to this node stays on this machine.
    pub fn is_local(&self) -> bool {
        self.is_local
    }
}

/// Nodes available to this runtime, by name.
#[derive(Default)]
pub struct NodeRegistry {
    nodes: BTreeMap<String, NodeConfig>,
    acp_profiles: BTreeMap<String, VisionAcpProfile>,
}

impl NodeRegistry {
    pub fn new(nodes: BTreeMap<String, NodeConfig>) -> Self {
        Self {
            nodes,
            acp_profiles: BTreeMap::new(),
        }
    }

    /// Builds the registry from configuration.
    ///
    /// A `[vision]` endpoint with no `[nodes]` table is carried forward as a
    /// node named `vision`. When both are present `[nodes]` wins and `[vision]`
    /// is ignored: merging two sources of truth for one endpoint is how a
    /// session ends up talking to a provider nobody chose.
    pub fn from_config(config: &AppConfig) -> Self {
        if !config.nodes.is_empty() {
            if config.vision.endpoint_url.is_some() {
                tracing::warn!(
                    "both [nodes] and [vision] are configured; [vision] is ignored. \
                     Move it into [nodes.<name>] with kind = \"vision\"."
                );
            }
            return Self {
                nodes: config.nodes.clone(),
                acp_profiles: config.vision.acp_profiles.clone(),
            };
        }
        let mut nodes = BTreeMap::new();
        if let Some(endpoint_url) = config.vision.endpoint_url.clone() {
            nodes.insert(
                LEGACY_VISION_NODE.to_owned(),
                NodeConfig {
                    kind: NodeKind::Vision,
                    endpoint_url,
                    token_env: config.vision.token_env.clone(),
                    timeout_ms: config.vision.timeout_ms,
                },
            );
        }
        Self {
            nodes,
            acp_profiles: config.vision.acp_profiles.clone(),
        }
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.nodes
            .keys()
            .chain(self.acp_profiles.keys())
            .map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.acp_profiles.is_empty()
    }

    /// Resolves `name` and checks it speaks `kind`.
    pub fn resolve(&self, name: &str, kind: NodeKind) -> Result<ResolvedNode, NodeError> {
        if self.acp_profiles.contains_key(name) {
            return Ok(ResolvedNode {
                name: name.to_owned(),
                kind,
                is_local: false,
            });
        }
        let node = self
            .nodes
            .get(name)
            .ok_or_else(|| NodeError::Unknown(name.to_owned()))?;
        if node.kind != kind {
            return Err(NodeError::WrongKind {
                name: name.to_owned(),
                configured: node.kind,
                requested: kind,
            });
        }
        Ok(ResolvedNode {
            name: name.to_owned(),
            kind: node.kind,
            is_local: node.is_local(),
        })
    }

    /// Builds a vision provider for the named node.
    ///
    /// There is deliberately no default-provider branch: a session that names
    /// no node has no vision provider, which the intent engine's
    /// deny-by-default double gate turns into a declined escalation.
    pub fn vision(&self, name: &str) -> Result<Arc<dyn VisionAssist>, NodeError> {
        if let Some(profile) = self.acp_profiles.get(name) {
            let assist = acp_client::AcpVisionAssist::new(
                profile.command.clone(),
                profile.args.clone(),
            )
            .with_auth_strategy(vision_auth_strategy(profile.auth));
            tracing::debug!(node = name, "node.vision.acp_resolved");
            return Ok(Arc::new(assist));
        }
        let resolved = self.resolve(name, NodeKind::Vision)?;
        let node = self.nodes.get(name).expect("resolve found it");
        let bearer = node
            .token_env
            .as_ref()
            .and_then(|variable| std::env::var(variable).ok());
        let assist = HttpVisionAssist::new(
            node.endpoint_url.clone(),
            bearer,
            Duration::from_millis(node.timeout_ms),
        )
        .map_err(|error| NodeError::Unreachable {
            name: name.to_owned(),
            reason: error.message,
        })?;
        tracing::debug!(
            node = %resolved.name,
            local = resolved.is_local(),
            "node.vision.resolved"
        );
        Ok(Arc::new(assist))
    }
}

/// The name a legacy `[vision]` endpoint is carried forward under.
pub const LEGACY_VISION_NODE: &str = "vision";

pub fn vision_auth_strategy(kind: VisionAuthKind) -> AuthStrategy {
    match kind {
        VisionAuthKind::Advertised => AuthStrategy::Advertised,
        VisionAuthKind::OAuthAuthorizationCode => AuthStrategy::OAuthAuthorizationCode,
        VisionAuthKind::OAuthDeviceCode => AuthStrategy::OAuthDeviceCode,
        VisionAuthKind::Environment => AuthStrategy::Environment,
        VisionAuthKind::ExistingSession => AuthStrategy::ExistingSession,
        VisionAuthKind::None => AuthStrategy::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::VisionConfig;

    fn node(kind: NodeKind, endpoint_url: &str) -> NodeConfig {
        NodeConfig {
            kind,
            endpoint_url: endpoint_url.to_owned(),
            token_env: None,
            timeout_ms: 15_000,
        }
    }

    fn registry(pairs: &[(&str, NodeConfig)]) -> NodeRegistry {
        NodeRegistry::new(
            pairs
                .iter()
                .map(|(name, config)| ((*name).to_owned(), config.clone()))
                .collect(),
        )
    }

    #[test]
    fn an_unconfigured_name_declines_instead_of_falling_back() {
        let registry = registry(&[(
            "remote",
            node(NodeKind::Vision, "https://vision.example/propose"),
        )]);
        assert_eq!(
            registry.resolve("local", NodeKind::Vision),
            Err(NodeError::Unknown("local".to_owned())),
            "an unknown name must not resolve to the one configured node"
        );
    }

    #[test]
    fn locality_comes_from_the_address() {
        let registry = registry(&[
            ("local", node(NodeKind::Vision, "http://127.0.0.1:8080/p")),
            (
                "also-local",
                node(NodeKind::Vision, "http://localhost:8080/p"),
            ),
            ("remote", node(NodeKind::Vision, "https://vision.example/p")),
            // A hostname that merely *contains* a loopback literal is remote.
            (
                "impostor",
                node(NodeKind::Vision, "https://127.0.0.1.example.com/p"),
            ),
        ]);
        for (name, expected) in [
            ("local", true),
            ("also-local", true),
            ("remote", false),
            ("impostor", false),
        ] {
            assert_eq!(
                registry
                    .resolve(name, NodeKind::Vision)
                    .expect("configured")
                    .is_local(),
                expected,
                "{name} locality"
            );
        }
    }

    #[test]
    fn an_unparseable_address_is_not_treated_as_local() {
        let registry = registry(&[("broken", node(NodeKind::Vision, "not a url"))]);
        assert!(
            !registry
                .resolve("broken", NodeKind::Vision)
                .expect("configured")
                .is_local(),
            "an address that cannot be parsed has not been shown to be local"
        );
    }

    #[test]
    fn a_legacy_vision_endpoint_is_carried_forward_under_a_name() {
        let config = AppConfig {
            vision: VisionConfig {
                endpoint_url: Some("http://127.0.0.1:8080/propose".to_owned()),
                token_env: None,
                timeout_ms: 9_000,
                ..VisionConfig::default()
            },
            ..AppConfig::default()
        };
        let registry = NodeRegistry::from_config(&config);
        let resolved = registry
            .resolve(LEGACY_VISION_NODE, NodeKind::Vision)
            .expect("legacy endpoint is reachable by name");
        assert!(resolved.is_local());
    }

    #[test]
    fn an_explicit_nodes_table_wins_over_a_legacy_vision_endpoint() {
        let mut config = AppConfig {
            vision: VisionConfig {
                endpoint_url: Some("https://legacy.example/propose".to_owned()),
                token_env: None,
                timeout_ms: 9_000,
                ..VisionConfig::default()
            },
            ..AppConfig::default()
        };
        config.nodes.insert(
            "local".to_owned(),
            node(NodeKind::Vision, "http://127.0.0.1:8080/propose"),
        );
        let registry = NodeRegistry::from_config(&config);
        assert_eq!(registry.names().collect::<Vec<_>>(), vec!["local"]);
        assert_eq!(
            registry.resolve(LEGACY_VISION_NODE, NodeKind::Vision),
            Err(NodeError::Unknown(LEGACY_VISION_NODE.to_owned())),
            "the ignored [vision] endpoint must not remain reachable"
        );
    }

    #[test]
    fn no_configuration_means_no_nodes() {
        let registry = NodeRegistry::from_config(&AppConfig::default());
        assert!(registry.is_empty());
        assert_eq!(
            registry.resolve("anything", NodeKind::Vision),
            Err(NodeError::Unknown("anything".to_owned()))
        );
    }

    #[test]
    fn every_vision_auth_kind_maps_to_distinct_auth_strategy() {
        use auth_broker::AuthStrategy::*;
        assert_eq!(
            vision_auth_strategy(VisionAuthKind::Advertised),
            Advertised
        );
        assert_eq!(
            vision_auth_strategy(VisionAuthKind::OAuthAuthorizationCode),
            OAuthAuthorizationCode
        );
        assert_eq!(
            vision_auth_strategy(VisionAuthKind::OAuthDeviceCode),
            OAuthDeviceCode
        );
        assert_eq!(
            vision_auth_strategy(VisionAuthKind::Environment),
            Environment
        );
        assert_eq!(
            vision_auth_strategy(VisionAuthKind::ExistingSession),
            ExistingSession
        );
        assert_eq!(vision_auth_strategy(VisionAuthKind::None), None);
    }

    #[test]
    fn acp_profile_is_a_named_vision_node() {
        let config = AppConfig::from_toml_str(
            r#"
[vision]
backend = "acp"
profile = "codex"
[vision.acp_profiles.codex]
command = "codex"
args = ["acp"]
auth = "advertised"
"#,
        )
        .unwrap();
        let registry = NodeRegistry::from_config(&config);
        assert_eq!(registry.names().collect::<Vec<_>>(), ["codex"]);
        assert!(registry.vision("codex").is_ok());
    }

    #[test]
    fn acp_oauth_auth_kinds_wire_distinct_strategies() {
        let oauth_code = AppConfig::from_toml_str(
            r#"
[vision]
backend = "acp"
profile = "codex"
[vision.acp_profiles.codex]
command = "codex"
args = ["acp"]
auth = "oauth-authorization-code"
"#,
        )
        .unwrap();
        let oauth_device = AppConfig::from_toml_str(
            r#"
[vision]
backend = "acp"
profile = "codex"
[vision.acp_profiles.codex]
command = "codex"
args = ["acp"]
auth = "oauth-device-code"
"#,
        )
        .unwrap();
        let code_profile = oauth_code.vision.acp_profiles.get("codex").unwrap();
        let device_profile = oauth_device.vision.acp_profiles.get("codex").unwrap();
        let code_assist = acp_client::AcpVisionAssist::new(
            code_profile.command.clone(),
            code_profile.args.clone(),
        )
        .with_auth_strategy(vision_auth_strategy(code_profile.auth));
        let device_assist = acp_client::AcpVisionAssist::new(
            device_profile.command.clone(),
            device_profile.args.clone(),
        )
        .with_auth_strategy(vision_auth_strategy(device_profile.auth));
        assert_eq!(
            code_assist.auth_strategy(),
            vision_auth_strategy(VisionAuthKind::OAuthAuthorizationCode)
        );
        assert_eq!(
            device_assist.auth_strategy(),
            vision_auth_strategy(VisionAuthKind::OAuthDeviceCode)
        );
        assert_ne!(code_assist.auth_strategy(), device_assist.auth_strategy());
        assert!(NodeRegistry::from_config(&oauth_code).vision("codex").is_ok());
        assert!(NodeRegistry::from_config(&oauth_device).vision("codex").is_ok());
    }
}
