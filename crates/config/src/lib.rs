use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub browser: BrowserConfig,
    pub storage: StorageConfig,
    #[serde(default)]
    pub http: HttpConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterfaceConfig {
    pub max_request_bytes: usize,
    pub max_event_batch: usize,
    pub max_event_retention: usize,
    pub max_connections: usize,
    pub token_records_path: PathBuf,
}

impl Default for InterfaceConfig {
    fn default() -> Self {
        Self {
            max_request_bytes: 1024 * 1024,
            max_event_batch: 256,
            max_event_retention: 16_384,
            max_connections: 64,
            token_records_path: PathBuf::from("./data/storage/authorities.json"),
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
        if self.token_records_path.as_os_str().is_empty() {
            return Err("interface token_records_path must not be empty");
        }
        Ok(())
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
    #[serde(default)]
    pub interface: InterfaceConfig,
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
            interface: InterfaceConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub journal_path: PathBuf,
    pub checkpoints_dir: PathBuf,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                host: "127.0.0.1".to_string(),
                port: 7777,
            },
            browser: BrowserConfig {
                executable: None,
                profiles_dir: PathBuf::from("./data/profiles"),
                headless: true,
                max_active: 8,
                upload_roots: vec![PathBuf::from("./data/uploads")],
                downloads_dir: PathBuf::from("./data/downloads"),
                artifacts_dir: PathBuf::from("./data/artifacts"),
                max_artifact_bytes: 8 * 1024 * 1024,
                max_screenshot_dimension: 16_384,
            },
            storage: StorageConfig {
                journal_path: PathBuf::from("./data/storage/commands.jsonl"),
                checkpoints_dir: PathBuf::from("./data/storage/checkpoints"),
            },
            http: HttpConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AppConfig, InterfaceConfig};

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
        let interface = InterfaceConfig::default();

        assert_eq!(interface.max_request_bytes, 1024 * 1024);
        assert_eq!(interface.max_event_batch, 256);
        assert_eq!(interface.max_event_retention, 16_384);
        assert_eq!(interface.max_connections, 64);
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
                token_records_path: std::path::PathBuf::new(),
                ..InterfaceConfig::default()
            },
        ] {
            assert!(invalid.validate().is_err());
        }
    }
}
