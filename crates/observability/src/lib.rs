use tracing_subscriber::{fmt, prelude::*, EnvFilter};

pub mod locks;
mod operational_metrics;

pub use operational_metrics::*;

pub mod fields {
    //! Standard field names. Bearer tokens, page content, and JS source are
    //! NEVER valid field values.
    pub const CORRELATION_ID: &str = "correlation_id";
    pub const PRINCIPAL_HASH: &str = "principal_hash";
    pub const SESSION_ID: &str = "session_id";
    pub const INTERFACE_VERSION: &str = "interface_version";

    /// First 16 hex chars of the SHA-256 of the principal id string.
    pub fn principal_hash(principal_id: &str) -> String {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(principal_id.as_bytes());
        hex::encode(&digest[..8])
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ObservabilityError {
    #[error("invalid observability level filter: {0}")]
    InvalidLevel(String),
}

/// Dropping the guard emits a final `runtime.shutdown` event.
pub struct ObservabilityGuard {
    _private: (),
}

impl Drop for ObservabilityGuard {
    fn drop(&mut self) {
        tracing::info!("runtime.shutdown");
    }
}

pub fn init(
    config: &config::ObservabilityConfig,
) -> Result<ObservabilityGuard, ObservabilityError> {
    let directive = std::env::var("RUST_LOG").unwrap_or_else(|_| config.level.clone());
    let filter = EnvFilter::try_new(&directive)
        .map_err(|_| ObservabilityError::InvalidLevel(directive.clone()))?;
    let registry = tracing_subscriber::registry().with(filter);
    match (config.format, config.sink) {
        (config::LogFormat::Json, config::LogSink::Stdout) => {
            registry
                .with(
                    fmt::layer()
                        .json()
                        .with_current_span(true)
                        .with_span_list(true)
                        .with_writer(std::io::stdout),
                )
                .init();
        }
        (config::LogFormat::Pretty, config::LogSink::Stdout) => {
            registry
                .with(fmt::layer().pretty().with_writer(std::io::stdout))
                .init();
        }
    }
    Ok(ObservabilityGuard { _private: () })
}

/// Stdio-safe init for the MCP stdio gateways: stdout is the protocol
/// channel there, so the only safe sink is stderr. Honors `RUST_LOG`; a
/// missing or unparseable directive disables logging rather than failing
/// startup, since this gate must never take the gateway down with it.
pub fn init_stdio() -> Option<ObservabilityGuard> {
    let directive = std::env::var("RUST_LOG").ok()?;
    let filter = EnvFilter::try_new(&directive).ok()?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init()
        .ok()?;
    Some(ObservabilityGuard { _private: () })
}

pub mod test_support {
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
    use tracing_subscriber::prelude::*;

    type Records = Arc<Mutex<Vec<serde_json::Value>>>;

    fn capture_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn active_records() -> &'static Mutex<Option<Records>> {
        static ACTIVE: OnceLock<Mutex<Option<Records>>> = OnceLock::new();
        ACTIVE.get_or_init(|| Mutex::new(None))
    }

    fn install_global_subscriber() {
        static INSTALLED: OnceLock<()> = OnceLock::new();
        INSTALLED.get_or_init(|| {
            let layer = tracing_subscriber::fmt::layer()
                .json()
                .with_current_span(true)
                .with_span_list(true)
                .with_writer(|| CaptureWriter {
                    records: active_records()
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone(),
                    buffer: Vec::new(),
                });
            tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer))
                .expect("capture subscriber must be the test process global default");
        });
    }

    /// In-memory sink for tests. A process-global subscriber observes events
    /// regardless of which executor thread polls the request, while this guard
    /// allows only one test at a time to retain those events.
    pub struct CaptureSink {
        records: Records,
        _capture_guard: MutexGuard<'static, ()>,
    }

    impl CaptureSink {
        pub fn install() -> Self {
            let capture_guard = capture_lock()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            install_global_subscriber();
            let records = Arc::new(Mutex::new(Vec::new()));
            *active_records()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(records.clone());
            Self {
                records,
                _capture_guard: capture_guard,
            }
        }

        pub fn events(&self) -> Vec<serde_json::Value> {
            self.records.lock().expect("capture sink mutex").clone()
        }
    }

    impl Drop for CaptureSink {
        fn drop(&mut self) {
            *active_records()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        }
    }

    struct CaptureWriter {
        records: Option<Records>,
        buffer: Vec<u8>,
    }

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.buffer.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Drop for CaptureWriter {
        fn drop(&mut self) {
            let values = self
                .buffer
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok());
            if let Some(records) = &self.records {
                records.lock().expect("capture sink mutex").extend(values);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::Write;

        #[test]
        fn capture_writer_retains_json_split_across_write_calls() {
            let records = Arc::new(Mutex::new(Vec::new()));
            let mut writer = CaptureWriter {
                records: Some(records.clone()),
                buffer: Vec::new(),
            };

            writer
                .write_all(br#"{"fields":{"message":"principal."#)
                .unwrap();
            writer.write_all(b"issued\"}}\n").unwrap();
            drop(writer);

            let events = records.lock().expect("capture sink mutex");
            assert_eq!(events.len(), 1);
            assert_eq!(events[0]["fields"]["message"], "principal.issued");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_info_json_stdout() {
        let config = config::ObservabilityConfig::default();
        assert_eq!(config.level, "info");
        assert_eq!(config.format, config::LogFormat::Json);
        assert_eq!(config.sink, config::LogSink::Stdout);
    }

    #[test]
    fn invalid_level_filter_is_rejected() {
        // `init` honors RUST_LOG over the configured level; clear it so the
        // configured (invalid) directive is the one that gets validated.
        std::env::remove_var("RUST_LOG");
        let config = config::ObservabilityConfig {
            level: "not a valid filter [".to_string(),
            ..Default::default()
        };
        assert!(init(&config).is_err());
    }

    #[test]
    fn capture_sink_records_json_events_with_fields() {
        let sink = test_support::CaptureSink::install();
        tracing::info!(correlation_id = "abc-123", "test event");
        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["fields"]["message"], "test event");
        assert_eq!(events[0]["fields"]["correlation_id"], "abc-123");
    }
}
