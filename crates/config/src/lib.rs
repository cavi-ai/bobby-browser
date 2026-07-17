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
    use super::AppConfig;

    #[test]
    fn http_defaults_deny_private_destinations_and_bound_concurrency() {
        let http = AppConfig::default().http;
        assert!(!http.allow_loopback);
        assert!(!http.allow_private_network);
        assert_eq!(http.max_concurrent_requests, 8);
        assert!(http.max_redirects > 0);
        assert!(http.request_timeout_ms > 0);
    }
}
