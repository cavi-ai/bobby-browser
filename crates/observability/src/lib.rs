use tracing_subscriber::{fmt, prelude::*, EnvFilter};

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
                .with(fmt::layer().json().with_writer(std::io::stdout))
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

pub mod test_support {
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::prelude::*;

    /// In-memory sink for tests. `install` uses `set_default`, so it never
    /// conflicts with global init and is scoped to the calling thread.
    /// Tests are short-lived; the subscriber default is intentionally kept
    /// for the remainder of the test.
    #[derive(Clone, Default)]
    pub struct CaptureSink {
        records: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    impl CaptureSink {
        pub fn install() -> Self {
            let sink = Self::default();
            let records = sink.records.clone();
            let layer = tracing_subscriber::fmt::layer()
                .json()
                .with_writer(move || CaptureWriter(records.clone()));
            let subscriber = tracing_subscriber::registry().with(layer);
            let guard = tracing::subscriber::set_default(subscriber);
            std::mem::forget(guard);
            sink
        }

        pub fn events(&self) -> Vec<serde_json::Value> {
            self.records.lock().expect("capture sink mutex").clone()
        }
    }

    struct CaptureWriter(Arc<Mutex<Vec<serde_json::Value>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(buf) {
                self.0.lock().expect("capture sink mutex").push(value);
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
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
