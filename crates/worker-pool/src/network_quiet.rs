//! CDP-backed in-flight request tracking for [`WaitCondition::NetworkQuiet`].
//!
//! Chromiumoxide enables the Network domain on every target, but does not
//! expose an in-flight query API. This module owns a page-scoped tracker fed
//! by CDP Network events and applies URL / resource-type / long-lived ignore
//! predicates when counting requests that should block a quiet wait.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chromiumoxide::cdp::browser_protocol::network::{
    EventLoadingFailed, EventLoadingFinished, EventRequestWillBeSent, EventWebSocketClosed,
    EventWebSocketCreated, RequestId, ResourceType,
};
use chromiumoxide::Page;
use futures::StreamExt;
use tokio::sync::Mutex;
use types::NetworkResourceType;

/// Requests open at least this long are treated as long-lived (long-poll /
/// streaming stand-ins) when `ignore_long_lived` is set.
pub const MAX_TRACKED_REQUESTS: usize = 4096;
const MAX_REQUEST_ID_BYTES: usize = 1024;
const MAX_REQUEST_URL_BYTES: usize = 16 * 1024;

pub const LONG_LIVED_OPEN_THRESHOLD: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct InFlightRequest {
    pub url: String,
    pub resource_type: NetworkResourceType,
    pub started_at: Instant,
    pub is_websocket: bool,
}

#[derive(Debug, Clone, Default)]
pub struct NetworkQuietFilters<'a> {
    pub ignore_url_substrings: &'a [String],
    pub ignore_resource_types: &'a [NetworkResourceType],
    pub ignore_long_lived: bool,
}

#[derive(Debug, Default)]
pub struct NetworkQuietState {
    requests: HashMap<String, InFlightRequest>,
    /// Request IDs whose finish/fail arrived before `requestWillBeSent` was
    /// applied (listener tasks can reorder CDP events).
    completed_before_start: HashSet<String>,
    tracking_lost: bool,
}

impl NetworkQuietState {
    pub fn upsert_http(
        &mut self,
        request_id: &RequestId,
        url: String,
        resource_type: NetworkResourceType,
    ) {
        self.upsert_id(request_id_key(request_id), url, resource_type);
    }

    pub fn upsert_websocket(&mut self, request_id: &RequestId, url: String) {
        self.upsert_id(
            request_id_key(request_id),
            url,
            NetworkResourceType::WebSocket,
        );
    }

    pub fn upsert_id(&mut self, id: String, url: String, resource_type: NetworkResourceType) {
        if self.completed_before_start.remove(&id) {
            return;
        }
        if id.len() > MAX_REQUEST_ID_BYTES
            || url.len() > MAX_REQUEST_URL_BYTES
            || (self.requests.len() >= MAX_TRACKED_REQUESTS && !self.requests.contains_key(&id))
        {
            self.mark_tracking_lost();
            return;
        }
        let is_websocket = matches!(
            resource_type,
            NetworkResourceType::WebSocket | NetworkResourceType::EventSource
        );
        self.requests.insert(
            id,
            InFlightRequest {
                url,
                resource_type,
                started_at: Instant::now(),
                is_websocket,
            },
        );
    }

    pub fn remove(&mut self, request_id: &RequestId) {
        self.remove_id(&request_id_key(request_id));
    }

    pub fn remove_id(&mut self, id: &str) {
        if self.requests.remove(id).is_none() {
            if id.len() > MAX_REQUEST_ID_BYTES
                || self.completed_before_start.len() >= MAX_TRACKED_REQUESTS
            {
                self.mark_tracking_lost();
            } else {
                self.completed_before_start.insert(id.to_owned());
            }
        }
    }

    pub fn mark_tracking_lost(&mut self) {
        if !self.tracking_lost {
            tracing::warn!("network event tracking lost; network quiet can no longer be certified for this tracker");
        }
        self.tracking_lost = true;
    }

    pub fn requests(&self) -> impl Iterator<Item = &InFlightRequest> {
        self.requests.values()
    }
}

/// Counts in-flight requests that are *not* excluded by `filters`, and returns
/// the set of exclusion class labels that matched at least one request.
pub fn counted_in_flight(
    state: &NetworkQuietState,
    filters: &NetworkQuietFilters<'_>,
    now: Instant,
) -> (usize, Vec<String>) {
    let mut excluded = BTreeSet::new();
    let mut count = usize::from(state.tracking_lost);
    if state.tracking_lost {
        excluded.insert("trackingLost".to_string());
    }
    for request in state.requests() {
        if let Some(class) = exclusion_class(request, filters, now) {
            excluded.insert(class);
            continue;
        }
        count += 1;
    }
    (count, excluded.into_iter().collect())
}

/// Scripts in flight under the long-lived threshold, and fetch/XHR open for
/// less than the settle cap; a longer fetch is a stream or a poll.
pub fn pending_page_loads(state: &NetworkQuietState, now: Instant) -> usize {
    state
        .requests()
        .filter(|request| {
            let open = now.duration_since(request.started_at);
            match request.resource_type {
                NetworkResourceType::Script => open < LONG_LIVED_OPEN_THRESHOLD,
                NetworkResourceType::Fetch | NetworkResourceType::Xhr => {
                    open < crate::navigation_settle::NAVIGATION_SETTLE_CAP
                }
                _ => false,
            }
        })
        .count()
}

fn exclusion_class(
    request: &InFlightRequest,
    filters: &NetworkQuietFilters<'_>,
    now: Instant,
) -> Option<String> {
    for substring in filters.ignore_url_substrings {
        if !substring.is_empty() && request.url.contains(substring) {
            return Some(format!("urlSubstring:{substring}"));
        }
    }
    if filters
        .ignore_resource_types
        .iter()
        .any(|wanted| wanted == &request.resource_type)
    {
        return Some(format!("resourceType:{}", request.resource_type.as_str()));
    }
    if filters.ignore_long_lived {
        if request.is_websocket
            || matches!(
                request.resource_type,
                NetworkResourceType::WebSocket | NetworkResourceType::EventSource
            )
        {
            return Some(match request.resource_type {
                NetworkResourceType::EventSource => "eventSource".into(),
                _ => "websocket".into(),
            });
        }
        if now.duration_since(request.started_at) >= LONG_LIVED_OPEN_THRESHOLD {
            return Some("longLived".into());
        }
    }
    None
}

fn request_id_key(request_id: &RequestId) -> String {
    request_id.inner().clone()
}

fn map_resource_type(value: Option<&ResourceType>) -> NetworkResourceType {
    match value {
        Some(ResourceType::Document) => NetworkResourceType::Document,
        Some(ResourceType::Stylesheet) => NetworkResourceType::Stylesheet,
        Some(ResourceType::Image) => NetworkResourceType::Image,
        Some(ResourceType::Media) => NetworkResourceType::Media,
        Some(ResourceType::Font) => NetworkResourceType::Font,
        Some(ResourceType::Script) => NetworkResourceType::Script,
        Some(ResourceType::TextTrack) => NetworkResourceType::TextTrack,
        Some(ResourceType::Xhr) => NetworkResourceType::Xhr,
        Some(ResourceType::Fetch) => NetworkResourceType::Fetch,
        Some(ResourceType::Prefetch) => NetworkResourceType::Prefetch,
        Some(ResourceType::EventSource) => NetworkResourceType::EventSource,
        Some(ResourceType::WebSocket) => NetworkResourceType::WebSocket,
        Some(ResourceType::Manifest) => NetworkResourceType::Manifest,
        Some(ResourceType::SignedExchange) => NetworkResourceType::SignedExchange,
        Some(ResourceType::Ping) => NetworkResourceType::Ping,
        Some(ResourceType::CspViolationReport) => NetworkResourceType::CspViolationReport,
        Some(ResourceType::Preflight) => NetworkResourceType::Preflight,
        Some(ResourceType::FedCm) => NetworkResourceType::FedCm,
        Some(ResourceType::Other) | None => NetworkResourceType::Other,
    }
}

/// Page-scoped tracker that listens to CDP Network events until dropped.
pub struct NetworkQuietTracker {
    state: Arc<Mutex<NetworkQuietState>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl NetworkQuietTracker {
    pub async fn start(page: &Page) -> Result<Arc<Self>, chromiumoxide::error::CdpError> {
        let state = Arc::new(Mutex::new(NetworkQuietState::default()));
        let mut tasks = Vec::new();

        let mut will_be_sent = page.event_listener::<EventRequestWillBeSent>().await?;
        let mut finished = page.event_listener::<EventLoadingFinished>().await?;
        let mut failed = page.event_listener::<EventLoadingFailed>().await?;
        let mut ws_created = page.event_listener::<EventWebSocketCreated>().await?;
        let mut ws_closed = page.event_listener::<EventWebSocketClosed>().await?;

        let state_will = Arc::clone(&state);
        tasks.push(tokio::spawn(async move {
            while let Some(event) = will_be_sent.next().await {
                let url = event.request.url.clone();
                let resource_type = map_resource_type(event.r#type.as_ref());
                state_will
                    .lock()
                    .await
                    .upsert_http(&event.request_id, url, resource_type);
            }
            state_will.lock().await.mark_tracking_lost();
        }));

        let state_finished = Arc::clone(&state);
        tasks.push(tokio::spawn(async move {
            while let Some(event) = finished.next().await {
                state_finished.lock().await.remove(&event.request_id);
            }
            state_finished.lock().await.mark_tracking_lost();
        }));

        let state_failed = Arc::clone(&state);
        tasks.push(tokio::spawn(async move {
            while let Some(event) = failed.next().await {
                state_failed.lock().await.remove(&event.request_id);
            }
            state_failed.lock().await.mark_tracking_lost();
        }));

        let state_ws_created = Arc::clone(&state);
        tasks.push(tokio::spawn(async move {
            while let Some(event) = ws_created.next().await {
                state_ws_created
                    .lock()
                    .await
                    .upsert_websocket(&event.request_id, event.url.clone());
            }
            state_ws_created.lock().await.mark_tracking_lost();
        }));

        let state_ws_closed = Arc::clone(&state);
        tasks.push(tokio::spawn(async move {
            while let Some(event) = ws_closed.next().await {
                state_ws_closed.lock().await.remove(&event.request_id);
            }
            state_ws_closed.lock().await.mark_tracking_lost();
        }));

        Ok(Arc::new(Self { state, tasks }))
    }

    pub async fn snapshot(&self, filters: &NetworkQuietFilters<'_>) -> (usize, Vec<String>) {
        let state = self.state.lock().await;
        counted_in_flight(&state, filters, Instant::now())
    }

    pub async fn pending_page_loads(&self) -> usize {
        pending_page_loads(&*self.state.lock().await, Instant::now())
    }
}

impl Drop for NetworkQuietTracker {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Parses a CDP resource-type string into our bounded enum, defaulting to Other.
#[allow(dead_code)]
pub fn parse_resource_type(raw: &str) -> NetworkResourceType {
    NetworkResourceType::from_str(raw).unwrap_or(NetworkResourceType::Other)
}

/// Maps WebDriver BiDi `destination` / `initiatorType` onto the same resource
/// types Chromium's Network domain uses, so `networkQuiet` filters match.
pub fn map_bidi_network_type(
    destination: Option<&str>,
    initiator_type: Option<&str>,
) -> NetworkResourceType {
    let destination = destination.unwrap_or("").to_ascii_lowercase();
    let initiator = initiator_type.unwrap_or("").to_ascii_lowercase();
    if initiator == "websocket" || destination == "websocket" {
        return NetworkResourceType::WebSocket;
    }
    if initiator == "eventsource" {
        return NetworkResourceType::EventSource;
    }
    if initiator == "xmlhttprequest" {
        return NetworkResourceType::Xhr;
    }
    if initiator == "fetch" {
        return NetworkResourceType::Fetch;
    }
    if initiator == "preflight" {
        return NetworkResourceType::Preflight;
    }
    match destination.as_str() {
        "document" | "frame" | "iframe" => NetworkResourceType::Document,
        "style" => NetworkResourceType::Stylesheet,
        "image" => NetworkResourceType::Image,
        "audio" | "video" => NetworkResourceType::Media,
        "font" => NetworkResourceType::Font,
        "script" => NetworkResourceType::Script,
        "track" => NetworkResourceType::TextTrack,
        "manifest" => NetworkResourceType::Manifest,
        "report" => NetworkResourceType::CspViolationReport,
        _ => NetworkResourceType::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(
        url: &str,
        resource_type: NetworkResourceType,
        started_at: Instant,
        is_websocket: bool,
    ) -> InFlightRequest {
        InFlightRequest {
            url: url.into(),
            resource_type,
            started_at,
            is_websocket,
        }
    }

    fn state_with(requests: Vec<InFlightRequest>) -> NetworkQuietState {
        let mut state = NetworkQuietState::default();
        for (index, request) in requests.into_iter().enumerate() {
            state.requests.insert(index.to_string(), request);
        }
        state
    }

    #[test]
    fn counts_all_requests_when_no_filters() {
        let now = Instant::now();
        let state = state_with(vec![
            request("https://a/x", NetworkResourceType::Xhr, now, false),
            request("https://b/y", NetworkResourceType::Fetch, now, false),
        ]);
        let filters = NetworkQuietFilters::default();
        let (count, excluded) = counted_in_flight(&state, &filters, now);
        assert_eq!(count, 2);
        assert!(excluded.is_empty());
    }

    #[test]
    fn ignores_url_substrings_and_records_class() {
        let now = Instant::now();
        let state = state_with(vec![
            request(
                "https://cdn.example/analytics.js",
                NetworkResourceType::Script,
                now,
                false,
            ),
            request(
                "https://api.example/data",
                NetworkResourceType::Xhr,
                now,
                false,
            ),
        ]);
        let ignore = vec!["analytics".to_owned()];
        let filters = NetworkQuietFilters {
            ignore_url_substrings: &ignore,
            ignore_resource_types: &[],
            ignore_long_lived: false,
        };
        let (count, excluded) = counted_in_flight(&state, &filters, now);
        assert_eq!(count, 1);
        assert_eq!(excluded, vec!["urlSubstring:analytics".to_owned()]);
    }

    #[test]
    fn ignores_resource_types() {
        let now = Instant::now();
        let state = state_with(vec![
            request("https://a/img.png", NetworkResourceType::Image, now, false),
            request("https://a/data", NetworkResourceType::Fetch, now, false),
        ]);
        let ignore_types = vec![NetworkResourceType::Image];
        let filters = NetworkQuietFilters {
            ignore_url_substrings: &[],
            ignore_resource_types: &ignore_types,
            ignore_long_lived: false,
        };
        let (count, excluded) = counted_in_flight(&state, &filters, now);
        assert_eq!(count, 1);
        assert_eq!(excluded, vec!["resourceType:Image".to_owned()]);
    }

    #[test]
    fn page_loads_count_scripts_and_recent_fetches_only() {
        let now = Instant::now();
        let stream = now - crate::navigation_settle::NAVIGATION_SETTLE_CAP;
        let state = state_with(vec![
            request(
                "https://a/app.js",
                NetworkResourceType::Script,
                stream,
                false,
            ),
            request("https://a/data", NetworkResourceType::Fetch, now, false),
            request("https://a/list", NetworkResourceType::Xhr, now, false),
            request(
                "https://a/stream",
                NetworkResourceType::Fetch,
                stream,
                false,
            ),
            request("https://a/img.png", NetworkResourceType::Image, now, false),
            request("wss://a/socket", NetworkResourceType::WebSocket, now, true),
        ]);
        assert_eq!(pending_page_loads(&state, now), 3);
    }

    #[test]
    fn ignores_websockets_and_long_open_requests_when_long_lived() {
        let now = Instant::now();
        let old = now - LONG_LIVED_OPEN_THRESHOLD - Duration::from_secs(1);
        let state = state_with(vec![
            request("wss://a/socket", NetworkResourceType::WebSocket, now, true),
            request(
                "https://a/events",
                NetworkResourceType::EventSource,
                now,
                false,
            ),
            request("https://a/long-poll", NetworkResourceType::Xhr, old, false),
            request("https://a/quick", NetworkResourceType::Fetch, now, false),
        ]);
        let filters = NetworkQuietFilters {
            ignore_url_substrings: &[],
            ignore_resource_types: &[],
            ignore_long_lived: true,
        };
        let (count, excluded) = counted_in_flight(&state, &filters, now);
        assert_eq!(count, 1);
        assert_eq!(
            excluded,
            vec![
                "eventSource".to_owned(),
                "longLived".to_owned(),
                "websocket".to_owned(),
            ]
        );
    }

    #[test]
    fn finish_before_will_be_sent_does_not_leave_orphan() {
        let id = RequestId::new("req-1".to_owned());
        let mut state = NetworkQuietState::default();
        state.remove(&id);
        state.upsert_http(
            &id,
            "https://example.test/ping".into(),
            NetworkResourceType::Fetch,
        );
        assert_eq!(state.requests().count(), 0);
        assert!(state.completed_before_start.is_empty());
    }

    #[test]
    fn bidi_destination_and_initiator_map_onto_chromium_resource_types() {
        assert_eq!(
            map_bidi_network_type(Some("script"), Some("parser")),
            NetworkResourceType::Script
        );
        assert_eq!(
            map_bidi_network_type(Some("empty"), Some("fetch")),
            NetworkResourceType::Fetch
        );
        assert_eq!(
            map_bidi_network_type(Some(""), Some("xmlhttprequest")),
            NetworkResourceType::Xhr
        );
        assert_eq!(
            map_bidi_network_type(None, Some("websocket")),
            NetworkResourceType::WebSocket
        );
        assert_eq!(
            map_bidi_network_type(Some("image"), None),
            NetworkResourceType::Image
        );
        assert_eq!(
            map_bidi_network_type(Some("unknown"), None),
            NetworkResourceType::Other
        );
    }
}

#[cfg(test)]
mod retention_tests {
    use super::*;
    #[test]
    fn unmatched_finishes_are_bounded_and_lost_tracking_cannot_claim_quiet() {
        let mut state = NetworkQuietState::default();
        for index in 0..10_000 {
            state.remove_id(&format!("missing-{index}"));
        }
        assert!(state.completed_before_start.len() <= 4096);
        let (count, _) = counted_in_flight(&state, &NetworkQuietFilters::default(), Instant::now());
        assert!(count > 0, "lost events cannot certify network quiet");
    }
    #[test]
    fn never_finishing_requests_are_bounded_even_when_filters_ignore_everything() {
        let mut state = NetworkQuietState::default();
        for index in 0..10_000 {
            state.upsert_id(
                format!("request-{index}"),
                "https://example.test".into(),
                NetworkResourceType::Fetch,
            );
        }
        assert!(state.requests.len() <= 4096);
        let ignored = vec![NetworkResourceType::Fetch];
        let filters = NetworkQuietFilters {
            ignore_resource_types: &ignored,
            ..NetworkQuietFilters::default()
        };
        assert!(counted_in_flight(&state, &filters, Instant::now()).0 > 0);
    }
}
