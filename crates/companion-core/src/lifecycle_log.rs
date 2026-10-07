//! Append-only lifecycle log for the Firefox native host: start, connect,
//! disconnect with its reason, rejected messages, fatal error, exit. It holds
//! event names and protocol metadata only, never page data or secrets.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// A log past this size moves to `<path>.1` when the next host starts.
const MAX_LOG_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Default)]
pub struct LifecycleLog {
    file: Option<Mutex<File>>,
}

impl LifecycleLog {
    pub fn disabled() -> Self {
        Self::default()
    }

    /// A log that cannot be opened is disabled: logging never stops the host.
    pub fn open(path: &Path) -> Self {
        if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > MAX_LOG_BYTES) {
            let mut rotated = path.as_os_str().to_owned();
            rotated.push(".1");
            let _ = std::fs::rename(path, rotated);
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        Self {
            file: options.open(path).ok().map(Mutex::new),
        }
    }

    pub fn record(&self, event: &str, detail: &str) {
        let Some(file) = &self.file else {
            return;
        };
        let unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or_default();
        let line = serde_json::json!({
            "unixMs": unix_ms,
            "pid": std::process::id(),
            "event": event,
            "detail": detail,
        });
        if let Ok(mut file) = file.lock() {
            let _ = writeln!(file, "{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_one_json_line_per_event_and_rotates_past_the_cap() {
        let dir = std::env::temp_dir().join(format!("lifecycle-log-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("host.log");
        let log = LifecycleLog::open(&path);
        log.record("start", "v1");
        log.record("disconnected", "server closed");
        let text = std::fs::read_to_string(&path).expect("read log");
        let lines = text
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json line"))
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["event"], "disconnected");
        assert_eq!(lines[1]["detail"], "server closed");
        assert_eq!(lines[1]["pid"], std::process::id());

        std::fs::write(&path, vec![b'x'; MAX_LOG_BYTES as usize + 1]).expect("fill log");
        LifecycleLog::open(&path).record("start", "v2");
        assert!(dir.join("host.log.1").exists());
        assert_eq!(
            std::fs::read_to_string(&path)
                .expect("read log")
                .lines()
                .count(),
            1
        );
        LifecycleLog::disabled().record("start", "ignored");
    }
}
