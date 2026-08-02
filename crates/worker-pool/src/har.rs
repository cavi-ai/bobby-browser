use std::collections::VecDeque;

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

pub fn har_document(entries: &[HarEntry], page_url: &str) -> Value {
    let items: Vec<Value> = entries
        .iter()
        .map(|entry| {
            json!({
                "startedDateTime": entry.started_unix_ms,
                "time": entry.elapsed_ms.unwrap_or(0.0),
                "request": {"method": entry.method, "url": entry.url, "httpVersion": "HTTP/1.1", "headers": [], "queryString": [], "cookies": [], "headersSize": -1, "bodySize": -1},
                "response": {
                    "status": entry.status.unwrap_or(0),
                    "statusText": entry.error_text.clone().unwrap_or_default(),
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
            "pages": [{"startedDateTime": entries.first().map_or(0.0, |entry| entry.started_unix_ms), "id": "page-1", "title": page_url, "pageTimings": {}}],
            "entries": items,
        }
    })
}
