use std::collections::VecDeque;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::RwLock;

const DEFAULT_CAPACITY: usize = 512;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HarEntry {
    pub url: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_text: Option<String>,
    pub started_unix_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transfer_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_text: Option<String>,
}

/// Bounded per-page network recorder backing the `network_log` primitive.
/// Each worker keeps one; the oldest entries drop past capacity.
pub struct HarRecorder {
    entries: RwLock<VecDeque<HarEntry>>,
    capacity: usize,
}

impl Default for HarRecorder {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}

impl HarRecorder {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: RwLock::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    pub async fn record(&self, entry: HarEntry) {
        let mut entries = self.entries.write().await;
        if entries.len() >= self.capacity {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    pub async fn take(&self, clear: bool) -> Vec<HarEntry> {
        if clear {
            let mut entries = self.entries.write().await;
            entries.drain(..).collect()
        } else {
            self.entries.read().await.iter().cloned().collect()
        }
    }
}

fn iso_8601_from_unix_ms(unix_ms: f64) -> String {
    let millis = if unix_ms.is_finite() {
        unix_ms.round() as i64
    } else {
        0
    };
    DateTime::<Utc>::from_timestamp_millis(millis)
        .unwrap_or(DateTime::UNIX_EPOCH)
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn har_document(entries: &[HarEntry], page_url: &str) -> Value {
    let items: Vec<Value> = entries
        .iter()
        .map(|entry| {
            let started_date_time = iso_8601_from_unix_ms(entry.started_unix_ms);
            json!({
                "startedDateTime": started_date_time,
                "time": entry.elapsed_ms.unwrap_or(0.0),
                "request": {"method": entry.method, "url": entry.url, "httpVersion": "HTTP/1.1", "headers": [], "queryString": [], "cookies": [], "headersSize": -1, "bodySize": -1},
                "response": {
                    "status": entry.status.unwrap_or(0),
                    "statusText": entry.status_text.clone().or_else(|| entry.error_text.clone()).unwrap_or_default(),
                    "httpVersion": "HTTP/1.1",
                    "headers": [],
                    "cookies": [],
                    "content": {"size": entry.transfer_bytes.unwrap_or(0), "mimeType": entry.mime_type.clone().unwrap_or_default()},
                    "redirectURL": "",
                    "headersSize": -1,
                    "bodySize": entry.transfer_bytes.map(|bytes| bytes as i64).unwrap_or(-1),
                },
                "cache": {},
                "timings": {"send": 0, "wait": entry.elapsed_ms.unwrap_or(0.0), "receive": 0},
            })
        })
        .collect();
    json!({
        "log": {
            "version": "1.2",
            "creator": {"name": "bobby-browser", "version": env!("CARGO_PKG_VERSION")},
            "pages": [{"startedDateTime": iso_8601_from_unix_ms(entries.first().map_or(0.0, |entry| entry.started_unix_ms)), "id": "page-1", "title": page_url, "pageTimings": {}}],
            "entries": items,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{har_document, HarEntry};

    #[test]
    fn har_started_datetimes_are_iso_8601_strings() {
        let document = har_document(
            &[HarEntry {
                url: "https://example.test/".into(),
                method: "GET".into(),
                status: Some(200),
                status_text: Some("OK".into()),
                started_unix_ms: 1_700_000_000_123.0,
                elapsed_ms: Some(12.0),
                transfer_bytes: Some(3),
                mime_type: Some("text/plain".into()),
                error_text: None,
            }],
            "https://example.test/",
        );

        assert_eq!(
            document["log"]["entries"][0]["startedDateTime"],
            "2023-11-14T22:13:20.123Z"
        );
        assert_eq!(
            document["log"]["pages"][0]["startedDateTime"],
            "2023-11-14T22:13:20.123Z"
        );
    }
}
