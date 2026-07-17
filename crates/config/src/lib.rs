use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub browser: BrowserConfig,
    pub storage: StorageConfig,
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
            },
            storage: StorageConfig {
                journal_path: PathBuf::from("./data/storage/commands.jsonl"),
                checkpoints_dir: PathBuf::from("./data/storage/checkpoints"),
            },
        }
    }
}
