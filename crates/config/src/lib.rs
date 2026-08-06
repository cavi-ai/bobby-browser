mod vision;
mod vision_write;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub use vision::{
    NodeConfig, NodeKind, VisionAcpProfile, VisionAuthKind, VisionBackendKind,
    VisionBackendSelection, VisionConfig, VisionDirectProfile, VisionProviderConfig,
};
pub use vision_write::{
    ensure_loopback_vision_defaults, upsert_vision_acp_profile, upsert_vision_platform,
    ConfigWriteError,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BrowserEngineConfig {
    Firefox,
    Chromium,
    WebKit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum EnginePreferenceConfig {
    ManagedChromium,
    Exact {
        engine: BrowserEngineConfig,
        #[serde(default, rename = "profileId")]
        profile_id: Option<String>,
    },
    Prefer {
        engines: Vec<BrowserEngineConfig>,
    },
}

impl Default for EnginePreferenceConfig {
    fn default() -> Self {
        Self::Exact {
            engine: BrowserEngineConfig::Firefox,
            profile_id: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserSelectionConfig {
    #[serde(default)]
    pub preference: EnginePreferenceConfig,
    #[serde(default)]
    pub firefox: Vec<FirefoxCompanionConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FirefoxCompanionConfig {
    pub profile_id: String,
    pub bidi_url: String,
    pub profile_dir: PathBuf,
    pub companion_bind: String,
    pub descriptor_path: PathBuf,
    #[serde(default = "default_companion_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_pairing_code_ttl_ms")]
    pub pairing_code_ttl_ms: u64,
    #[serde(default = "default_attachment_ttl_ms")]
    pub attachment_ttl_ms: u64,
}

const fn default_companion_timeout_ms() -> u64 {
    5_000
}
const fn default_pairing_code_ttl_ms() -> u64 {
    300_000
}
const fn default_attachment_ttl_ms() -> u64 {
    300_000
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub browser: BrowserConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub http: HttpConfig,
    #[serde(default)]
    pub interface: InterfaceConfig,
    #[serde(default)]
    pub observability: ObservabilityConfig,
    #[serde(default)]
    pub vision: VisionConfig,
    #[serde(default)]
    pub context: ContextConfig,
    /// Named nodes, selected per session. Absent means no node is reachable.
    #[serde(default)]
    pub nodes: std::collections::BTreeMap<String, NodeConfig>,
    #[serde(default)]
    pub cdp: CdpConfig,
    #[serde(default)]
    pub mcp: McpConfig,
}

/// MCP gateway presentation settings. Nothing here is an enforcement
/// boundary -- capability gates remain the only one.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpConfig {
    /// Toolset phase a connection starts in: `full` (default), `explore`,
    /// `act`, `intent`, or `verify`. A narrow phase cuts the `tools/list`
    /// handshake substantially -- the full surface is ~127 KB, `explore` is
    /// ~42 KB -- which the agent would otherwise pay before its first call.
    /// Hidden tools stay callable, and the agent can widen with
    /// `toolset_select` at any time. `BOBBY_MCP_TOOLSET` overrides this.
    #[serde(default)]
    pub startup_toolset: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CdpConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
}

impl Default for CdpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: "127.0.0.1".to_string(),
            port: 9222,
        }
    }
}

/// Durable shared-context-graph configuration (Spec C). The store only ever
/// opens for runtimes whose engine selection carries a durable profile
/// identity (Firefox companion); `dir` unset disables promotion entirely.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextConfig {
    /// Store root; profile id is appended as a subdirectory. The CLI fills
    /// `<config-dir>/bobby-browser/context` when unset.
    #[serde(default)]
    pub dir: Option<PathBuf>,
    /// Days a control record is kept without a successful verification.
    #[serde(default = "default_context_ttl_days")]
    pub ttl_days: u32,
}

const fn default_context_ttl_days() -> u32 {
    90
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterfaceConfig {
    #[serde(default = "default_max_request_bytes", alias = "maxRequestBytes")]
    pub max_request_bytes: usize,
    #[serde(default = "default_max_event_batch", alias = "maxEventBatch")]
    pub max_event_batch: usize,
    #[serde(default = "default_max_event_retention", alias = "maxEventRetention")]
    pub max_event_retention: usize,
    #[serde(default = "default_max_connections", alias = "maxConnections")]
    pub max_connections: usize,
    #[serde(
        default = "default_max_rejection_workers",
        alias = "maxRejectionWorkers"
    )]
    pub max_rejection_workers: usize,
    #[serde(default = "default_token_records_path", alias = "tokenRecordsPath")]
    pub token_records_path: PathBuf,
    #[serde(default = "default_max_principals", alias = "maxPrincipals")]
    pub max_principals: usize,
    #[serde(
        default = "default_max_in_flight_per_principal",
        alias = "maxInFlightPerPrincipal"
    )]
    pub max_in_flight_per_principal: usize,
}

const fn default_max_request_bytes() -> usize {
    1024 * 1024
}

const fn default_max_event_batch() -> usize {
    256
}

const fn default_max_event_retention() -> usize {
    16_384
}

const fn default_max_connections() -> usize {
    64
}

const fn default_max_rejection_workers() -> usize {
    16
}

fn default_token_records_path() -> PathBuf {
    PathBuf::from("./data/storage/authorities.json")
}

const fn default_max_principals() -> usize {
    16
}

const fn default_max_in_flight_per_principal() -> usize {
    8
}

impl Default for InterfaceConfig {
    fn default() -> Self {
        Self {
            max_request_bytes: default_max_request_bytes(),
            max_event_batch: default_max_event_batch(),
            max_event_retention: default_max_event_retention(),
            max_connections: default_max_connections(),
            max_rejection_workers: default_max_rejection_workers(),
            token_records_path: default_token_records_path(),
            max_principals: default_max_principals(),
            max_in_flight_per_principal: default_max_in_flight_per_principal(),
        }
    }
}

impl InterfaceConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.max_request_bytes == 0 {
            return Err("interface max_request_bytes must be positive");
        }
        if self.max_event_batch == 0 {
            return Err("interface max_event_batch must be positive");
        }
        if self.max_event_retention == 0 {
            return Err("interface max_event_retention must be positive");
        }
        if self.max_connections == 0 {
            return Err("interface max_connections must be positive");
        }
        if self.max_rejection_workers == 0 {
            return Err("interface max_rejection_workers must be positive");
        }
        if self.token_records_path.as_os_str().is_empty() {
            return Err("interface token_records_path must not be empty");
        }
        if self.max_principals == 0 {
            return Err("interface max_principals must be positive");
        }
        if self.max_in_flight_per_principal == 0 {
            return Err("interface max_in_flight_per_principal must be positive");
        }
        Ok(())
    }
}

/// Error returned when loading or parsing an [`AppConfig`] from TOML fails.
#[derive(Debug)]
pub enum ConfigLoadError {
    /// The config file could not be read; a missing file is not an error,
    /// see [`AppConfig::load`].
    Io(std::io::Error),
    /// The file contents were not valid TOML, or did not match the
    /// [`AppConfig`] schema.
    Parse(toml::de::Error),
    /// The parsed config failed [`AppConfig::validate`].
    Invalid(&'static str),
}

impl std::fmt::Display for ConfigLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigLoadError::Io(err) => write!(f, "failed to read config file: {err}"),
            ConfigLoadError::Parse(err) => write!(f, "failed to parse config file: {err}"),
            ConfigLoadError::Invalid(reason) => write!(f, "invalid config: {reason}"),
        }
    }
}

impl std::error::Error for ConfigLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigLoadError::Io(err) => Some(err),
            ConfigLoadError::Parse(err) => Some(err),
            ConfigLoadError::Invalid(_) => None,
        }
    }
}

impl AppConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.interface.validate()?;
        if self.cdp.enabled {
            let loopback = self.cdp.host == "localhost"
                || self
                    .cdp
                    .host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback());
            if !loopback {
                return Err("cdp host must be loopback when enabled");
            }
            if self.cdp.port == self.server.port {
                return Err("cdp port must differ from server port when enabled");
            }
        }
        Ok(())
    }

    /// Parse an [`AppConfig`] from a TOML document and validate it.
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigLoadError> {
        let config: AppConfig = toml::from_str(text).map_err(ConfigLoadError::Parse)?;
        config.validate().map_err(ConfigLoadError::Invalid)?;
        Ok(config)
    }

    /// Load an [`AppConfig`] from a TOML file at `path`.
    ///
    /// A missing file is not an error: built-in defaults ([`AppConfig::default`])
    /// are returned in that case. Any other I/O failure, parse failure, or
    /// validation failure is returned as a [`ConfigLoadError`].
    pub fn load(path: &std::path::Path) -> Result<Self, ConfigLoadError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AppConfig::default());
            }
            Err(err) => return Err(ConfigLoadError::Io(err)),
        };
        Self::from_toml_str(&text)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    pub allow_loopback: bool,
    pub allow_private_network: bool,
    pub max_redirects: usize,
    pub max_header_bytes: usize,
    pub max_body_bytes: usize,
    pub max_download_bytes: usize,
    pub request_timeout_ms: u64,
    pub max_concurrent_requests: usize,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            allow_loopback: false,
            allow_private_network: false,
            max_redirects: 5,
            max_header_bytes: 64 * 1024,
            max_body_bytes: 8 * 1024 * 1024,
            max_download_bytes: 64 * 1024 * 1024,
            request_timeout_ms: 30_000,
            max_concurrent_requests: 8,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    #[serde(default = "default_shutdown_timeout_ms")]
    pub shutdown_timeout_ms: u64,
}

const fn default_shutdown_timeout_ms() -> u64 {
    10_000
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 7777,
            shutdown_timeout_ms: default_shutdown_timeout_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserConfig {
    pub executable: Option<PathBuf>,
    pub profiles_dir: PathBuf,
    pub headless: bool,
    pub max_active: usize,
    pub upload_roots: Vec<PathBuf>,
    pub downloads_dir: PathBuf,
    pub artifacts_dir: PathBuf,
    pub max_artifact_bytes: usize,
    pub max_screenshot_dimension: u32,
    pub max_js_result_bytes: usize,
    /// Upper bound, in milliseconds, on a single `EvaluateJavaScript` command's
    /// `timeout_ms`. Larger requests are clamped, not rejected, so a caller cannot
    /// pin a worker lease open indefinitely.
    pub max_js_timeout_ms: u64,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            executable: std::env::var_os("BOBBY_CHROME_EXECUTABLE")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            profiles_dir: PathBuf::from("./data/profiles"),
            headless: true,
            max_active: 8,
            upload_roots: vec![PathBuf::from("./data/uploads")],
            downloads_dir: PathBuf::from("./data/downloads"),
            artifacts_dir: PathBuf::from("./data/artifacts"),
            max_artifact_bytes: 8 * 1024 * 1024,
            max_screenshot_dimension: 16_384,
            max_js_result_bytes: 64 * 1024,
            max_js_timeout_ms: 30_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub journal_path: PathBuf,
    pub checkpoints_dir: PathBuf,
    pub authority_path: PathBuf,
    /// Append-only JSONL journal for the in-process task scheduler.
    #[serde(default = "default_scheduler_journal_path")]
    pub scheduler_journal_path: PathBuf,
}

fn default_scheduler_journal_path() -> PathBuf {
    PathBuf::from("./data/storage/scheduler-jobs.jsonl")
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            journal_path: PathBuf::from("./data/storage/commands.jsonl"),
            checkpoints_dir: PathBuf::from("./data/storage/checkpoints"),
            authority_path: PathBuf::from("./data/storage/authority.json"),
            scheduler_journal_path: default_scheduler_journal_path(),
        }
    }
}

#[cfg(test)]
mod tests {
    /// A node kind that does not exist must fail to load rather than parse
    /// into something.
    #[test]
    fn an_unknown_node_kind_is_rejected() {
        for kind in ["context", "Vision", "planner", ""] {
            let text = format!(
                "{}\n[nodes.helper]\nkind = \"{kind}\"\nendpoint_url = \"http://127.0.0.1:8081/x\"\n",
                MINIMAL_CONFIG
            );
            assert!(
                toml::from_str::<super::AppConfig>(&text).is_err(),
                "kind = {kind:?} parsed instead of being rejected"
            );
        }
    }

    #[test]
    fn a_vision_node_kind_loads() {
        let text = format!(
            "{}\n[nodes.helper]\nkind = \"vision\"\nendpoint_url = \"http://127.0.0.1:8081/x\"\n",
            MINIMAL_CONFIG
        );
        let config: super::AppConfig = toml::from_str(&text).expect("vision node loads");
        assert_eq!(config.nodes.len(), 1);
    }

    #[test]
    fn vision_prefill_defaults_off_and_round_trips() {
        let config = super::VisionConfig::default();
        assert!(!config.prefill, "prefill must default off");
        let text = r#"
[vision]
prefill = true
"#;
        let parsed: super::AppConfig = toml::from_str(text).expect("parse prefill flag");
        assert!(parsed.vision.prefill);
        let absent: super::AppConfig = toml::from_str("[vision]\n").expect("parse empty vision");
        assert!(!absent.vision.prefill);
    }

    #[test]
    fn vision_providers_table_loads_and_selects() {
        let text = r#"
[vision]
endpoint_url = "http://127.0.0.1:9100/vision"
token_env = "BOBBY_VISION_TOKEN"
provider = "lmstudio"

[vision.providers.lmstudio]
base_url = "http://127.0.0.1:1234/v1"
model = "local-model"
"#;
        let config: super::AppConfig = toml::from_str(text).expect("parse");
        let (name, profile) = config.vision.selected_provider().expect("selected");
        assert_eq!(name, "lmstudio");
        assert_eq!(profile.base_url, "http://127.0.0.1:1234/v1");
        assert_eq!(profile.model, "local-model");
        assert!(profile.api_key_env.is_none());
    }

    #[test]
    fn acp_backend_profile_loads_without_credentials() {
        let text = r#"
[vision]
backend = "acp"
profile = "codex"

[vision.acp_profiles.codex]
command = "codex"
args = ["acp"]
auth = "advertised"
"#;
        let config: super::AppConfig = toml::from_str(text).expect("parse ACP profile");
        let selected = config
            .vision
            .selected_backend()
            .expect("selected ACP backend");
        assert!(matches!(
            selected,
            super::VisionBackendSelection::Acp { name: "codex", .. }
        ));
    }

    #[test]
    fn legacy_provider_maps_to_direct_backend() {
        let text = r#"
[vision]
provider = "lmstudio"

[vision.providers.lmstudio]
base_url = "http://127.0.0.1:1234/v1"
model = "local-model"
"#;
        let config: super::AppConfig = toml::from_str(text).expect("parse legacy profile");
        let selected = config
            .vision
            .selected_backend()
            .expect("selected legacy backend");
        assert!(matches!(
            selected,
            super::VisionBackendSelection::LegacyDirect {
                name: "lmstudio",
                ..
            }
        ));
    }

    #[test]
    fn vision_provider_missing_from_table_returns_none() {
        let text = r#"
[vision]
provider = "missing"
"#;
        let config: super::AppConfig = toml::from_str(text).unwrap();
        assert!(config.vision.selected_provider().is_none());
    }

    #[test]
    fn nodes_still_load_alongside_vision_providers() {
        let text = r#"
[vision]
provider = "openai"
[vision.providers.openai]
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"

[nodes.helper]
kind = "vision"
endpoint_url = "http://127.0.0.1:8081/x"
"#;
        let config: super::AppConfig = toml::from_str(text).unwrap();
        assert!(config.nodes.contains_key("helper"));
        assert!(config.vision.selected_provider().is_some());
    }

    const MINIMAL_CONFIG: &str = r#"
[server]
host = "127.0.0.1"
port = 7777
shutdown_timeout_ms = 1000
[browser]
profiles_dir = "p"
headless = true
max_active = 1
upload_roots = []
downloads_dir = "d"
artifacts_dir = "a"
max_artifact_bytes = 1
max_screenshot_dimension = 1
max_js_result_bytes = 1
max_js_timeout_ms = 1
[storage]
journal_path = "j"
checkpoints_dir = "c"
authority_path = "au"
scheduler_journal_path = "s"
"#;

    use super::{
        AppConfig, BrowserEngineConfig, BrowserSelectionConfig, ConfigLoadError,
        EnginePreferenceConfig, InterfaceConfig,
    };

    #[test]
    fn missing_browser_selection_defaults_to_exact_firefox_without_fallback() {
        let parsed: BrowserSelectionConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(
            parsed.preference,
            EnginePreferenceConfig::Exact {
                engine: BrowserEngineConfig::Firefox,
                profile_id: None,
            }
        );
        assert!(parsed.firefox.is_empty());
    }

    #[test]
    fn exact_firefox_profile_and_ordered_preferences_parse_from_config() {
        let exact: BrowserSelectionConfig = serde_json::from_value(serde_json::json!({
            "preference": { "mode": "exact", "engine": "firefox", "profileId": "profile-a" },
            "firefox": [{
                "profileId": "profile-a",
                "bidiUrl": "ws://127.0.0.1:9222/session",
                "profileDir": "/profiles/default-release",
                "companionBind": "127.0.0.1:0",
                "descriptorPath": "./data/storage/firefox-companion.json"
            }]
        }))
        .unwrap();
        assert_eq!(
            exact.preference,
            EnginePreferenceConfig::Exact {
                engine: BrowserEngineConfig::Firefox,
                profile_id: Some("profile-a".into()),
            }
        );
        assert_eq!(exact.firefox.len(), 1);

        let prefer: BrowserSelectionConfig = serde_json::from_value(serde_json::json!({
            "preference": { "mode": "prefer", "engines": ["firefox", "chromium"] }
        }))
        .unwrap();
        assert_eq!(
            prefer.preference,
            EnginePreferenceConfig::Prefer {
                engines: vec![BrowserEngineConfig::Firefox, BrowserEngineConfig::Chromium],
            }
        );
    }

    #[test]
    fn http_defaults_deny_private_destinations_and_bound_concurrency() {
        let http = AppConfig::default().http;
        assert!(!http.allow_loopback);
        assert!(!http.allow_private_network);
        assert_eq!(http.max_concurrent_requests, 8);
        assert!(http.max_redirects > 0);
        assert!(http.request_timeout_ms > 0);
    }

    #[test]
    fn interface_defaults_are_bounded_and_store_only_an_authority_record_path() {
        let config = AppConfig::default();
        let interface = config.interface;

        assert_eq!(interface.max_request_bytes, 1024 * 1024);
        assert_eq!(interface.max_event_batch, 256);
        assert_eq!(interface.max_event_retention, 16_384);
        assert_eq!(interface.max_connections, 64);
        assert_eq!(interface.max_rejection_workers, 16);
        assert_eq!(interface.max_in_flight_per_principal, 8);
        assert_eq!(
            interface.token_records_path,
            std::path::PathBuf::from("./data/storage/authorities.json")
        );
        assert!(interface.validate().is_ok());
        assert!(!format!("{:?}", AppConfig::default())
            .to_ascii_lowercase()
            .contains("bearer"));
    }

    #[test]
    fn partial_interface_blocks_use_field_defaults_and_app_validation() {
        let baseline = AppConfig::default();
        let mut value = serde_json::to_value(&baseline).unwrap();
        value["interface"] = serde_json::json!({ "maxRequestBytes": 4096 });
        let parsed: AppConfig = serde_json::from_value(value).unwrap();

        assert_eq!(parsed.interface.max_request_bytes, 4096);
        assert_eq!(parsed.interface.max_event_batch, 256);
        assert!(parsed.validate().is_ok());

        let invalid = AppConfig {
            interface: InterfaceConfig {
                max_connections: 0,
                ..InterfaceConfig::default()
            },
            ..baseline
        };
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn interface_rejects_zero_bounds_and_an_empty_authority_path() {
        for invalid in [
            InterfaceConfig {
                max_request_bytes: 0,
                ..InterfaceConfig::default()
            },
            InterfaceConfig {
                max_event_batch: 0,
                ..InterfaceConfig::default()
            },
            InterfaceConfig {
                max_event_retention: 0,
                ..InterfaceConfig::default()
            },
            InterfaceConfig {
                max_connections: 0,
                ..InterfaceConfig::default()
            },
            InterfaceConfig {
                max_rejection_workers: 0,
                ..InterfaceConfig::default()
            },
            InterfaceConfig {
                token_records_path: std::path::PathBuf::new(),
                ..InterfaceConfig::default()
            },
            InterfaceConfig {
                max_in_flight_per_principal: 0,
                ..InterfaceConfig::default()
            },
        ] {
            assert!(invalid.validate().is_err());
        }
    }

    #[test]
    fn default_includes_scheduler_journal_path() {
        let config = AppConfig::default();
        assert_eq!(
            config.storage.scheduler_journal_path,
            std::path::PathBuf::from("./data/storage/scheduler-jobs.jsonl")
        );
    }

    #[test]
    fn cdp_defaults_disabled_on_loopback_9222() {
        let cfg = AppConfig::default();
        assert!(!cfg.cdp.enabled);
        assert_eq!(cfg.cdp.host, "127.0.0.1");
        assert_eq!(cfg.cdp.port, 9222);
    }

    #[test]
    fn cdp_table_parses_from_toml() {
        let cfg = AppConfig::from_toml_str(
            r#"
            [cdp]
            enabled = true
            host = "127.0.0.1"
            port = 9333
            "#,
        )
        .expect("parse");
        assert!(cfg.cdp.enabled);
        assert_eq!(cfg.cdp.port, 9333);
    }

    #[test]
    fn enabled_cdp_rejects_non_loopback_hosts() {
        let mut config = super::AppConfig::default();
        config.cdp.enabled = true;
        config.cdp.host = "0.0.0.0".into();
        assert_eq!(
            config.validate(),
            Err("cdp host must be loopback when enabled")
        );
    }

    #[test]
    fn enabled_cdp_rejects_the_http_port() {
        let mut config = super::AppConfig::default();
        config.cdp.enabled = true;
        config.cdp.port = config.server.port;
        assert_eq!(
            config.validate(),
            Err("cdp port must differ from server port when enabled")
        );
    }

    #[test]
    fn from_toml_str_round_trips_the_default_config() {
        let text = toml::to_string(&AppConfig::default()).unwrap();
        let parsed = AppConfig::from_toml_str(&text).unwrap();
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            serde_json::to_value(AppConfig::default()).unwrap()
        );
    }

    #[test]
    fn from_toml_str_rejects_a_config_that_fails_validation() {
        let invalid = AppConfig {
            interface: InterfaceConfig {
                max_principals: 0,
                ..InterfaceConfig::default()
            },
            ..AppConfig::default()
        };
        let text = toml::to_string(&invalid).unwrap();

        let err = AppConfig::from_toml_str(&text).unwrap_err();
        assert!(matches!(err, ConfigLoadError::Invalid(_)));
    }

    #[test]
    fn from_toml_str_reports_malformed_toml_as_a_parse_error() {
        let err = AppConfig::from_toml_str("not valid toml === [[[").unwrap_err();
        assert!(matches!(err, ConfigLoadError::Parse(_)));
    }

    #[test]
    fn load_of_a_missing_file_returns_defaults() {
        let path = std::path::Path::new("/nonexistent/definitely-not-there/config.toml");
        let config = AppConfig::load(path).unwrap();
        assert_eq!(
            serde_json::to_value(&config).unwrap(),
            serde_json::to_value(AppConfig::default()).unwrap()
        );
    }

    #[test]
    fn load_of_the_repo_config_toml_parses_and_validates() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config.toml");
        let config = AppConfig::load(&path).expect("repo config.toml must parse and validate");
        assert!(config.validate().is_ok());
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservabilityConfig {
    #[serde(default = "default_observability_level")]
    pub level: String,
    #[serde(default)]
    pub format: LogFormat,
    #[serde(default)]
    pub sink: LogSink,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            level: default_observability_level(),
            format: LogFormat::default(),
            sink: LogSink::default(),
        }
    }
}

fn default_observability_level() -> String {
    "info".to_string()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LogFormat {
    #[default]
    Json,
    Pretty,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LogSink {
    #[default]
    Stdout,
}
