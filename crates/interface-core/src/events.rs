use std::{collections::VecDeque, sync::Arc};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex, Notify};
use types::{EventCursor, PrincipalId};

const MAX_EVENT_KIND_BYTES: usize = 128;
pub const MAX_EVENT_PAYLOAD_BYTES: usize = 16 * 1024;
pub const MAX_EVENT_PAYLOAD_NODES: usize = 1024;
const MAX_PAYLOAD_DEPTH: usize = 8;
const MAX_PAYLOAD_ENTRIES: usize = 64;
const MAX_PAYLOAD_KEY_BYTES: usize = 128;
const MAX_PAYLOAD_STRING_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub cursor: EventCursor,
    pub kind: String,
    pub payload: Value,
}

impl Event {
    pub fn new(kind: impl Into<String>, payload: Value) -> Self {
        Self {
            cursor: EventCursor::ZERO,
            kind: kind.into(),
            payload,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventBatch {
    pub events: Vec<Event>,
    pub latest_available: EventCursor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EventGapReason {
    HistoryLost,
    InvalidLimit,
    InvalidCursor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventGap {
    pub reason: EventGapReason,
    pub earliest_available: EventCursor,
}

#[derive(Clone)]
pub struct EventStore {
    inner: Arc<EventStoreInner>,
}

struct EventStoreInner {
    capacity: usize,
    state: Mutex<EventState>,
    appended: Notify,
}

struct EventState {
    next_cursor: u64,
    retained: VecDeque<StoredEvent>,
}

struct StoredEvent {
    audience: Option<PrincipalId>,
    event: Event,
}

impl EventStore {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "event retention capacity must be positive");
        Self {
            inner: Arc::new(EventStoreInner {
                capacity,
                state: Mutex::new(EventState {
                    next_cursor: 1,
                    retained: VecDeque::with_capacity(capacity),
                }),
                appended: Notify::new(),
            }),
        }
    }

    pub async fn append(&self, event: Event) -> EventCursor {
        self.append_inner(None, event).await
    }

    pub async fn append_for(&self, principal: PrincipalId, event: Event) -> EventCursor {
        self.append_inner(Some(principal), event).await
    }

    async fn append_inner(&self, audience: Option<PrincipalId>, mut event: Event) -> EventCursor {
        event.kind = truncate_utf8(event.kind, MAX_EVENT_KIND_BYTES);
        sanitize_payload(&mut event.payload);

        let cursor = {
            let mut state = self.inner.state.lock().await;
            let cursor = EventCursor(state.next_cursor);
            state.next_cursor = state
                .next_cursor
                .checked_add(1)
                .expect("event cursor space exhausted");
            event.cursor = cursor;
            if state.retained.len() == self.inner.capacity {
                state.retained.pop_front();
            }
            state.retained.push_back(StoredEvent { audience, event });
            cursor
        };

        // Notification is deliberately outside the queue critical section.
        self.inner.appended.notify_waiters();
        cursor
    }

    /// The cursor of the most recently appended event, or [`EventCursor::ZERO`]
    /// when nothing has been appended yet.
    ///
    /// Exists for consumers that must start at the tail rather than replay
    /// history — a push stream whose protocol gives the client no way to name a
    /// resume cursor. Starting such a consumer at `ZERO` is not merely wasteful:
    /// `read_decision`'s `HistoryLost` test compares against the *store-wide*
    /// front of retention, so once the shared log has evicted a single entry,
    /// every cursor-`ZERO` read gaps immediately, forever.
    ///
    /// This is store-wide, not per principal, and deliberately so: it is a
    /// position to start reading *from*, and every read that follows still goes
    /// through [`EventStore::read_after_for`]. It exposes nothing a caller
    /// cannot already see — every successful batch already carries the same
    /// store-wide `latest_available`.
    pub async fn latest_cursor(&self) -> EventCursor {
        let state = self.inner.state.lock().await;
        EventCursor(state.next_cursor.saturating_sub(1))
    }

    pub async fn read_after(
        &self,
        cursor: EventCursor,
        limit: usize,
    ) -> Result<EventBatch, EventGap> {
        if limit == 0 {
            return Err(EventGap {
                reason: EventGapReason::InvalidLimit,
                earliest_available: EventCursor::ZERO,
            });
        }
        let limit = limit.min(self.inner.capacity);

        loop {
            // Register before inspecting state so an append cannot be lost between
            // releasing the queue lock and awaiting the notification.
            let notified = self.inner.appended.notified();
            let decision = {
                let state = self.inner.state.lock().await;
                read_decision(&state, None, cursor, limit)
            };
            match decision {
                ReadDecision::Return(result) => return result,
                ReadDecision::Wait => notified.await,
            }
        }
    }

    pub async fn read_after_for(
        &self,
        principal: &PrincipalId,
        cursor: EventCursor,
        limit: usize,
    ) -> Result<EventBatch, EventGap> {
        if limit == 0 {
            return Err(EventGap {
                reason: EventGapReason::InvalidLimit,
                earliest_available: EventCursor::ZERO,
            });
        }
        let limit = limit.min(self.inner.capacity);

        loop {
            let notified = self.inner.appended.notified();
            let decision = {
                let state = self.inner.state.lock().await;
                read_decision(&state, Some(principal), cursor, limit)
            };
            match decision {
                ReadDecision::Return(result) => return result,
                ReadDecision::Wait => notified.await,
            }
        }
    }
}

enum ReadDecision {
    Return(Result<EventBatch, EventGap>),
    Wait,
}

fn read_decision(
    state: &EventState,
    principal: Option<&PrincipalId>,
    cursor: EventCursor,
    limit: usize,
) -> ReadDecision {
    let latest = state
        .retained
        .back()
        .map_or(EventCursor::ZERO, |stored| stored.event.cursor);
    let earliest = state
        .retained
        .front()
        .map_or(EventCursor::ZERO, |stored| stored.event.cursor);

    if cursor > latest {
        return ReadDecision::Return(Err(EventGap {
            reason: EventGapReason::InvalidCursor,
            earliest_available: earliest,
        }));
    }
    if earliest.0 > cursor.0.saturating_add(1) {
        return ReadDecision::Return(Err(EventGap {
            reason: EventGapReason::HistoryLost,
            earliest_available: earliest,
        }));
    }

    let events = state
        .retained
        .iter()
        .filter(|stored| stored.event.cursor > cursor)
        .filter(|stored| {
            principal.is_none_or(|principal| stored.audience.as_ref() == Some(principal))
        })
        .take(limit)
        .map(|stored| stored.event.clone())
        .collect::<Vec<_>>();
    if events.is_empty() {
        ReadDecision::Wait
    } else {
        ReadDecision::Return(Ok(EventBatch {
            events,
            latest_available: latest,
        }))
    }
}

fn sanitize_payload(value: &mut Value) {
    let mut remaining_nodes = MAX_EVENT_PAYLOAD_NODES;
    sanitize_value(value, 0, &mut remaining_nodes);
    if serde_json::to_vec(value).map_or(true, |bytes| bytes.len() > MAX_EVENT_PAYLOAD_BYTES) {
        *value = serde_json::json!({ "truncated": true });
    }
}

fn sanitize_value(value: &mut Value, depth: usize, remaining_nodes: &mut usize) {
    if *remaining_nodes == 0 {
        *value = Value::String("[TRUNCATED]".to_owned());
        return;
    }
    *remaining_nodes -= 1;
    if depth >= MAX_PAYLOAD_DEPTH {
        *value = Value::String("[TRUNCATED]".to_owned());
        return;
    }
    match value {
        Value::String(string) => {
            *string = truncate_utf8(std::mem::take(string), MAX_PAYLOAD_STRING_BYTES);
        }
        Value::Array(values) => {
            values.truncate(MAX_PAYLOAD_ENTRIES);
            let mut retained = Vec::with_capacity(values.len().min(*remaining_nodes));
            for mut value in std::mem::take(values) {
                if *remaining_nodes == 0 {
                    break;
                }
                sanitize_value(&mut value, depth + 1, remaining_nodes);
                retained.push(value);
            }
            *values = retained;
        }
        Value::Object(values) => {
            let retained = std::mem::take(values);
            for (key, mut value) in retained.into_iter().take(MAX_PAYLOAD_ENTRIES) {
                if *remaining_nodes == 0 {
                    break;
                }
                let sensitive = is_sensitive_key(&key);
                let key = truncate_utf8(key, MAX_PAYLOAD_KEY_BYTES);
                if sensitive {
                    *remaining_nodes -= 1;
                    value = Value::String("[REDACTED]".to_owned());
                } else {
                    sanitize_value(&mut value, depth + 1, remaining_nodes);
                }
                values.entry(key).or_insert(value);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase();
    [
        "authorization",
        "cookie",
        "credential",
        "password",
        "secret",
        "token",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_count(value: &Value) -> usize {
        1 + match value {
            Value::Array(values) => values.iter().map(node_count).sum(),
            Value::Object(values) => values.values().map(node_count).sum(),
            _ => 0,
        }
    }

    #[test]
    fn redacted_values_consume_the_same_node_budget_as_other_values() {
        let mut payload = serde_json::json!({
            "token-a": "secret-a",
            "token-b": "secret-b",
            "token-c": "secret-c"
        });
        let mut remaining_nodes = 2;

        sanitize_value(&mut payload, 0, &mut remaining_nodes);

        assert!(node_count(&payload) <= 2);
        assert_eq!(remaining_nodes, 0);
        assert!(!payload.to_string().contains("secret"));
    }
}
