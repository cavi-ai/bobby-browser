//! Vision and named-node configuration surfaces.
//!
//! `[vision]` holds provider/ACP settings and an optional legacy HTTP endpoint.
//! `[nodes.<name>]` holds named HTTP vision nodes. Runtime merge policy lives in
//! `node_registry::NodeRegistry::from_config` — this module only owns the types.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Vision-assist provider configuration. Deny by default: no endpoint means
/// vision escalation is unavailable even when sessions and tokens opt in.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VisionConfig {
    #[serde(default, alias = "endpointUrl")]
    pub endpoint_url: Option<String>,
    /// Environment variable holding the provider bearer token. The token
    /// itself is never stored in the config file.
    #[serde(default, alias = "tokenEnv")]
    pub token_env: Option<String>,
    #[serde(default = "default_vision_timeout_ms", alias = "timeoutMs")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub providers: BTreeMap<String, VisionProviderConfig>,
    #[serde(default)]
    pub backend: Option<VisionBackendKind>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub fallback_profile: Option<String>,
    #[serde(default)]
    pub acp_profiles: BTreeMap<String, VisionAcpProfile>,
    #[serde(default)]
    pub provider_profiles: BTreeMap<String, VisionDirectProfile>,
    /// Lazy batch prefill: the first vision-eligible stuck field in a form
    /// proposes for every remaining field purpose from one screenshot and
    /// caches the results. Default off; the off path is byte-identical.
    #[serde(default)]
    pub prefill: bool,
    /// When set, every vision escalation (executed or rejected) appends one
    /// JSONL corpus record to `<corpus_dir>/vision-corpus.jsonl` with the
    /// screenshot, the exact candidate list sent to the model, the proposal,
    /// the terminal outcome, and — for verified clicks — the resolved target
    /// index. Default unset: no records are written.
    #[serde(default, alias = "corpusDir")]
    pub corpus_dir: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum VisionBackendKind {
    Acp,
    Direct,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum VisionAuthKind {
    Advertised,
    #[serde(rename = "oauth-authorization-code")]
    OAuthAuthorizationCode,
    #[serde(rename = "oauth-device-code")]
    OAuthDeviceCode,
    Environment,
    ExistingSession,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VisionAcpProfile {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub auth: VisionAuthKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VisionDirectProfile {
    pub kind: String,
    pub base_url: String,
    pub model: String,
    pub auth: VisionAuthKind,
    #[serde(default)]
    pub credential_handle: Option<String>,
    #[serde(default)]
    pub credential_env: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VisionProviderConfig {
    #[serde(alias = "baseUrl")]
    pub base_url: String,
    pub model: String,
    #[serde(default, alias = "apiKeyEnv")]
    pub api_key_env: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum VisionBackendSelection<'a> {
    Acp {
        name: &'a str,
        profile: &'a VisionAcpProfile,
    },
    Direct {
        name: &'a str,
        profile: &'a VisionDirectProfile,
    },
    LegacyDirect {
        name: &'a str,
        profile: &'a VisionProviderConfig,
    },
}

impl VisionConfig {
    pub fn selected_provider(&self) -> Option<(&str, &VisionProviderConfig)> {
        let name = self.provider.as_deref()?;
        let profile = self.providers.get(name)?;
        Some((name, profile))
    }

    pub fn selected_backend(&self) -> Option<VisionBackendSelection<'_>> {
        match self.backend {
            Some(VisionBackendKind::Acp) => {
                let name = self.profile.as_deref()?;
                self.acp_profiles
                    .get(name)
                    .map(|profile| VisionBackendSelection::Acp { name, profile })
            }
            Some(VisionBackendKind::Direct) => {
                let name = self.profile.as_deref()?;
                self.provider_profiles
                    .get(name)
                    .map(|profile| VisionBackendSelection::Direct { name, profile })
            }
            None => self
                .selected_provider()
                .map(|(name, profile)| VisionBackendSelection::LegacyDirect { name, profile }),
        }
    }
}

fn default_vision_timeout_ms() -> u64 {
    15_000
}

/// One addressable node: a separate process with a bounded contract, reached
/// over HTTP.
///
/// The privacy property comes from the node's *address*, not from trusting
/// whoever runs it: a loopback node cannot send page pixels or page text off
/// the machine, and [`NodeConfig::is_local`] is the check the runtime records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    /// What contract this node speaks.
    pub kind: NodeKind,
    #[serde(alias = "endpointUrl")]
    pub endpoint_url: String,
    /// Environment variable holding the node's bearer token. The token itself
    /// is never stored in the config file.
    #[serde(default, alias = "tokenEnv")]
    pub token_env: Option<String>,
    #[serde(default = "default_vision_timeout_ms", alias = "timeoutMs")]
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeKind {
    /// Proposes an action from a screenshot. The `VisionAssist` contract.
    Vision,
    // No `Context` variant: retained page structure is answered in-process by
    // `page_runtime::ContextGraph`, so `kind = "context"` would reach nothing.
    // An unknown kind fails to load with the legal values named.
}

impl NodeConfig {
    /// Whether this node's address is on the local machine.
    ///
    /// An address that cannot be parsed answers `false`: callers use this to
    /// decide whether page material leaves the machine.
    pub fn is_local(&self) -> bool {
        url::Url::parse(&self.endpoint_url).is_ok_and(|url| {
            matches!(
                url.host_str(),
                Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
            )
        })
    }
}
