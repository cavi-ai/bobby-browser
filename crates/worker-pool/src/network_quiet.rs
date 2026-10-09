//! CDP-backed in-flight request tracking for [`WaitCondition::NetworkQuiet`].
//!
//! Chromiumoxide enables the Network domain on every target, but does not
//! expose an in-flight query API. This module owns a page-scoped tracker fed
//! by CDP Network and frame events and applies URL / resource-type /
//! long-lived ignore predicates when counting requests that should block a
//! quiet wait.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chromiumoxide::cdp::browser_protocol::network::{
    EventLoadingFailed, EventLoadingFinished, EventRequestWillBeSent, EventWebSocketClosed,
    EventWebSocketCreated, RequestId, ResourceType,
};
use chromiumoxide::cdp::browser_protocol::page::{EventFrameDetached, EventFrameNavigated};
use chromiumoxide::Page;
use futures::StreamExt;
use tokio::sync::Mutex;
use types::NetworkResourceType;

/// Requests a tracker holds; past it the oldest is evicted.
pub const MAX_TRACKED_REQUESTS: usize = 4096;
/// Longer request IDs are keyed by a prefix and a hash of the whole ID.
const MAX_REQUEST_ID_BYTES: usize = 1024;
/// Longer URLs are tracked truncated.
const MAX_REQUEST_URL_BYTES: usize = 16 * 1024;

/// Requests open at least this long are treated as long-lived (long-poll /
/// streaming stand-ins) when `ignore_long_lived` is set.
pub const LONG_LIVED_OPEN_THRESHOLD: Duration = Duration::from_secs(30);
/// Replaced documents remembered so a request one of them starts late is not counted.
const MAX_RETIRED_LOADERS: usize = 64;

#[derive(Debug, Clone)]
pub struct InFlightRequest {
    pub url: String,
    pub resource_type: NetworkResourceType,
    pub started_at: Instant,
    pub is_websocket: bool,
    /// The frame and document that started the request, when Chromium names them.
    pub document: Option<RequestDocument>,
}

/// A Chromium frame and the loader of the document that started a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestDocument {
    pub frame: String,
    pub loader: String,
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
    completed_before_start: RecentIds,
    /// The loader of the document each frame shows.
    committed: HashMap<String, String>,
    /// Loaders of documents a frame replaced or dropped, oldest first.
    retired_loaders: VecDeque<String>,
    /// Requests that ended with their document; a late finish for one is expected.
    dropped: RecentIds,
    tracking_lost: bool,
}

/// Request IDs kept for one settle cap, at most [`MAX_TRACKED_REQUESTS`];
/// past it older entries are evicted first.
#[derive(Debug, Default)]
struct RecentIds(HashMap<String, Instant>);

impl RecentIds {
    fn insert(&mut self, id: String, now: Instant) {
        if self.0.len() >= MAX_TRACKED_REQUESTS && !self.0.contains_key(&id) {
            self.0.retain(|_, seen| {
                now.duration_since(*seen) < crate::navigation_settle::NAVIGATION_SETTLE_CAP
            });
            if self.0.len() >= MAX_TRACKED_REQUESTS {
                let oldest = self
                    .0
                    .iter()
                    .min_by_key(|(_, seen)| **seen)
                    .map(|(id, _)| id.clone());
                if let Some(oldest) = oldest {
                    self.0.remove(&oldest);
                }
            }
        }
        self.0.insert(id, now);
    }

    fn remove(&mut self, id: &str) -> bool {
        self.0.remove(id).is_some()
    }
}

/// Why a tracker missed network events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackingLoss {
    /// The event stream dropped events it could not buffer.
    Lagged,
    /// The event stream closed.
    StreamEnded,
}

impl TrackingLoss {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lagged => "lagged",
            Self::StreamEnded => "streamEnded",
        }
    }
}

/// Logs that a tracker missed network events.
pub fn warn_tracking_lost(reason: TrackingLoss) {
    tracing::warn!(
        reason = reason.as_str(),
        "network event tracking lost; network quiet is unknown until the next document commits"
    );
}

/// `id`, or for an ID over [`MAX_REQUEST_ID_BYTES`] a prefix of it and a hash
/// of the whole.
pub fn bounded_request_id(id: &str) -> String {
    if id.len() <= MAX_REQUEST_ID_BYTES {
        return id.to_owned();
    }
    let mut hasher = DefaultHasher::new();
    id.hash(&mut hasher);
    let mut end = MAX_REQUEST_ID_BYTES - 17;
    while !id.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}#{:016x}", &id[..end], hasher.finish())
}

fn bounded_url(mut url: String) -> String {
    if url.len() > MAX_REQUEST_URL_BYTES {
        let mut end = MAX_REQUEST_URL_BYTES;
        while !url.is_char_boundary(end) {
            end -= 1;
        }
        url.truncate(end);
    }
    url
}

impl NetworkQuietState {
    pub fn upsert_http(
        &mut self,
        request_id: &RequestId,
        url: String,
        resource_type: NetworkResourceType,
        document: Option<RequestDocument>,
    ) {
        let id = request_id_key(request_id);
        if document
            .as_ref()
            .is_some_and(|document| self.retired_loaders.contains(&document.loader))
        {
            if !self.completed_before_start.remove(&id) {
                self.forget(id);
            }
            return;
        }
        self.insert(id, url, resource_type, document);
    }

    pub fn upsert_websocket(&mut self, request_id: &RequestId, url: String) {
        self.insert(
            request_id_key(request_id),
            url,
            NetworkResourceType::WebSocket,
            None,
        );
    }

    pub fn upsert_id(&mut self, id: &str, url: String, resource_type: NetworkResourceType) {
        self.insert(bounded_request_id(id), url, resource_type, None);
    }

    /// `frame` now shows the document `loader` loaded. Requests its earlier
    /// documents started end with them; a navigation's document request stays.
    /// A `top` frame's commit also ends an earlier event gap.
    pub fn commit_document(&mut self, frame: &str, loader: &str, top: bool) {
        if top {
            self.tracking_lost = false;
        }
        self.retired_loaders.retain(|retired| retired != loader);
        if self.committed.len() < MAX_TRACKED_REQUESTS || self.committed.contains_key(frame) {
            if let Some(previous) = self.committed.insert(frame.to_owned(), loader.to_owned()) {
                if previous != loader {
                    self.retire_loader(previous);
                }
            }
        }
        self.end_requests(|request| {
            request.resource_type != NetworkResourceType::Document
                && request
                    .document
                    .as_ref()
                    .is_some_and(|document| document.frame == frame && document.loader != loader)
        });
    }

    /// `frame` left the page: every request it started ends with it.
    pub fn detach_frame(&mut self, frame: &str) {
        if let Some(loader) = self.committed.remove(frame) {
            self.retire_loader(loader);
        }
        self.end_requests(|request| {
            request
                .document
                .as_ref()
                .is_some_and(|document| document.frame == frame)
        });
    }

    fn end_requests(&mut self, ended: impl Fn(&InFlightRequest) -> bool) {
        let ids: Vec<String> = self
            .requests
            .iter()
            .filter(|(_, request)| ended(request))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(document) = self
                .requests
                .remove(&id)
                .and_then(|request| request.document)
            {
                self.retire_loader(document.loader);
            }
            self.forget(id);
        }
    }

    fn retire_loader(&mut self, loader: String) {
        if self.retired_loaders.contains(&loader) {
            return;
        }
        if self.retired_loaders.len() >= MAX_RETIRED_LOADERS {
            self.retired_loaders.pop_front();
        }
        self.retired_loaders.push_back(loader);
    }

    /// A context without frame events committed a document after a navigation
    /// that started at `navigation_started`: requests started before it ended
    /// with the old document, and an earlier event gap ends. Returns their IDs.
    pub fn commit_navigation(&mut self, navigation_started: Instant) -> Vec<String> {
        self.tracking_lost = false;
        let ended: Vec<String> = self
            .requests
            .iter()
            .filter(|(_, request)| request.started_at < navigation_started)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &ended {
            self.drop_request(id);
        }
        ended
    }

    /// Stops tracking `id`; a late finish for it is expected.
    pub fn drop_request(&mut self, id: &str) {
        if self.requests.remove(id).is_some() {
            self.forget(id.to_owned());
        }
    }

    fn forget(&mut self, id: String) {
        self.dropped.insert(id, Instant::now());
    }

    fn insert(
        &mut self,
        id: String,
        url: String,
        resource_type: NetworkResourceType,
        document: Option<RequestDocument>,
    ) {
        if self.completed_before_start.remove(&id) {
            return;
        }
        if self.requests.len() >= MAX_TRACKED_REQUESTS && !self.requests.contains_key(&id) {
            let oldest = self
                .requests
                .iter()
                .min_by_key(|(_, request)| request.started_at)
                .map(|(id, _)| id.clone());
            if let Some(oldest) = oldest {
                self.drop_request(&oldest);
            }
        }
        let is_websocket = matches!(
            resource_type,
            NetworkResourceType::WebSocket | NetworkResourceType::EventSource
        );
        self.requests.insert(
            id,
            InFlightRequest {
                url: bounded_url(url),
                resource_type,
                started_at: Instant::now(),
                is_websocket,
                document,
            },
        );
    }

    pub fn remove(&mut self, request_id: &RequestId) {
        self.remove_id(request_id.inner());
    }

    pub fn remove_id(&mut self, id: &str) {
        let id = bounded_request_id(id);
        if self.requests.remove(&id).is_none() && !self.dropped.remove(&id) {
            self.completed_before_start.insert(id, Instant::now());
        }
    }

    /// Events were missed: in-flight loads are unknown until a document
    /// commits. `true` when tracking was intact until now.
    pub fn mark_tracking_lost(&mut self) -> bool {
        !std::mem::replace(&mut self.tracking_lost, true)
    }

    fn stream_ended(&mut self) {
        if self.mark_tracking_lost() {
            warn_tracking_lost(TrackingLoss::StreamEnded);
        }
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
/// less than the settle cap; a longer fetch is a stream or a poll. `None`
/// while missed events leave them unknown.
pub fn pending_page_loads(state: &NetworkQuietState, now: Instant) -> Option<usize> {
    if state.tracking_lost {
        return None;
    }
    let pending = state
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
        .count();
    Some(pending)
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
    bounded_request_id(request_id.inner())
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
        let mut navigated = page.event_listener::<EventFrameNavigated>().await?;
        let mut detached = page.event_listener::<EventFrameDetached>().await?;

        let state_will = Arc::clone(&state);
        tasks.push(tokio::spawn(async move {
            while let Some(event) = will_be_sent.next().await {
                let url = event.request.url.clone();
                let resource_type = map_resource_type(event.r#type.as_ref());
                let document = event.frame_id.as_ref().map(|frame| RequestDocument {
                    frame: frame.inner().clone(),
                    loader: event.loader_id.inner().clone(),
                });
                state_will.lock().await.upsert_http(
                    &event.request_id,
                    url,
                    resource_type,
                    document,
                );
            }
            state_will.lock().await.stream_ended();
        }));

        let state_finished = Arc::clone(&state);
        tasks.push(tokio::spawn(async move {
            while let Some(event) = finished.next().await {
                state_finished.lock().await.remove(&event.request_id);
            }
            state_finished.lock().await.stream_ended();
        }));

        let state_failed = Arc::clone(&state);
        tasks.push(tokio::spawn(async move {
            while let Some(event) = failed.next().await {
                state_failed.lock().await.remove(&event.request_id);
            }
            state_failed.lock().await.stream_ended();
        }));

        let state_ws_created = Arc::clone(&state);
        tasks.push(tokio::spawn(async move {
            while let Some(event) = ws_created.next().await {
                state_ws_created
                    .lock()
                    .await
                    .upsert_websocket(&event.request_id, event.url.clone());
            }
            state_ws_created.lock().await.stream_ended();
        }));

        let state_ws_closed = Arc::clone(&state);
        tasks.push(tokio::spawn(async move {
            while let Some(event) = ws_closed.next().await {
                state_ws_closed.lock().await.remove(&event.request_id);
            }
            state_ws_closed.lock().await.stream_ended();
        }));

        // Chromium reports no loadingFailed for a request the next document
        // cancels; the frame's commit or detach ends it instead.
        let state_navigated = Arc::clone(&state);
        tasks.push(tokio::spawn(async move {
            while let Some(event) = navigated.next().await {
                state_navigated.lock().await.commit_document(
                    event.frame.id.inner(),
                    event.frame.loader_id.inner(),
                    event.frame.parent_id.is_none(),
                );
            }
            state_navigated.lock().await.stream_ended();
        }));

        let state_detached = Arc::clone(&state);
        tasks.push(tokio::spawn(async move {
            while let Some(event) = detached.next().await {
                state_detached
                    .lock()
                    .await
                    .detach_frame(event.frame_id.inner());
            }
            state_detached.lock().await.stream_ended();
        }));

        Ok(Arc::new(Self { state, tasks }))
    }

    pub async fn snapshot(&self, filters: &NetworkQuietFilters<'_>) -> (usize, Vec<String>) {
        let state = self.state.lock().await;
        counted_in_flight(&state, filters, Instant::now())
    }

    pub async fn pending_page_loads(&self) -> Option<usize> {
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
            document: None,
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
        assert_eq!(pending_page_loads(&state, now), Some(3));
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
            None,
        );
        assert_eq!(state.requests().count(), 0);
        assert!(state.completed_before_start.0.is_empty());
    }

    fn started_by(
        state: &mut NetworkQuietState,
        id: &str,
        resource_type: NetworkResourceType,
        frame: &str,
        loader: &str,
    ) {
        state.upsert_http(
            &RequestId::new(id.to_owned()),
            format!("https://example.test/{id}"),
            resource_type,
            Some(RequestDocument {
                frame: frame.into(),
                loader: loader.into(),
            }),
        );
    }

    fn tracked(state: &NetworkQuietState) -> Vec<String> {
        let mut urls: Vec<String> = state
            .requests()
            .map(|request| request.url.clone())
            .collect();
        urls.sort();
        urls
    }

    #[test]
    fn a_commit_ends_the_replaced_documents_requests() {
        let mut state = NetworkQuietState::default();
        state.commit_document("main", "first", true);
        started_by(
            &mut state,
            "data",
            NetworkResourceType::Fetch,
            "main",
            "first",
        );
        started_by(
            &mut state,
            "ad",
            NetworkResourceType::Script,
            "child",
            "inner",
        );
        started_by(
            &mut state,
            "next",
            NetworkResourceType::Document,
            "main",
            "second",
        );
        assert_eq!(pending_page_loads(&state, Instant::now()), Some(2));

        state.commit_document("main", "second", true);
        assert_eq!(
            tracked(&state),
            ["https://example.test/ad", "https://example.test/next"]
        );
        started_by(
            &mut state,
            "late",
            NetworkResourceType::Fetch,
            "main",
            "first",
        );
        state.remove(&RequestId::new("data".to_owned()));
        state.remove(&RequestId::new("late".to_owned()));
        assert!(state.completed_before_start.0.is_empty());
        assert!(state.dropped.0.is_empty());
        started_by(
            &mut state,
            "rows",
            NetworkResourceType::Fetch,
            "main",
            "second",
        );
        assert_eq!(pending_page_loads(&state, Instant::now()), Some(2));
    }

    #[test]
    fn a_detached_frame_ends_its_requests() {
        let mut state = NetworkQuietState::default();
        state.commit_document("child", "inner", false);
        started_by(
            &mut state,
            "ad",
            NetworkResourceType::Script,
            "child",
            "inner",
        );
        started_by(
            &mut state,
            "rows",
            NetworkResourceType::Fetch,
            "main",
            "top",
        );
        state.detach_frame("child");
        assert_eq!(tracked(&state), ["https://example.test/rows"]);
        started_by(
            &mut state,
            "late",
            NetworkResourceType::Xhr,
            "child",
            "inner",
        );
        assert_eq!(tracked(&state), ["https://example.test/rows"]);
    }

    #[test]
    fn a_restored_document_counts_its_requests_again() {
        let mut state = NetworkQuietState::default();
        state.commit_document("main", "first", true);
        state.commit_document("main", "second", true);
        state.commit_document("main", "first", true);
        started_by(
            &mut state,
            "data",
            NetworkResourceType::Fetch,
            "main",
            "first",
        );
        assert_eq!(tracked(&state), ["https://example.test/data"]);
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

    fn fetch(state: &mut NetworkQuietState, id: &str, url: String) {
        state.upsert_id(id, url, NetworkResourceType::Fetch);
    }

    #[test]
    fn unmatched_finishes_are_bounded_and_keep_tracking() {
        let mut state = NetworkQuietState::default();
        for index in 0..10_000 {
            state.remove_id(&format!("missing-{index}"));
        }
        assert_eq!(state.completed_before_start.0.len(), MAX_TRACKED_REQUESTS);
        assert_eq!(pending_page_loads(&state, Instant::now()), Some(0));
        fetch(&mut state, "rows", "https://example.test/rows".into());
        assert_eq!(pending_page_loads(&state, Instant::now()), Some(1));
        let (count, excluded) =
            counted_in_flight(&state, &NetworkQuietFilters::default(), Instant::now());
        assert_eq!((count, excluded), (1, Vec::<String>::new()));
    }

    #[test]
    fn unmatched_finishes_older_than_the_settle_cap_are_evicted_first() {
        let mut ids = RecentIds::default();
        let old = Instant::now();
        for index in 0..MAX_TRACKED_REQUESTS {
            ids.insert(format!("old-{index}"), old);
        }
        let later = old + crate::navigation_settle::NAVIGATION_SETTLE_CAP;
        ids.insert("new".into(), later);
        assert_eq!(ids.0.len(), 1);
        assert!(ids.remove("new"));
    }

    #[test]
    fn never_finishing_requests_evict_the_oldest_and_keep_counting() {
        let mut state = NetworkQuietState::default();
        for index in 0..5_000 {
            fetch(
                &mut state,
                &format!("request-{index}"),
                "https://example.test".into(),
            );
        }
        assert_eq!(state.requests.len(), MAX_TRACKED_REQUESTS);
        assert_eq!(
            pending_page_loads(&state, Instant::now()),
            Some(MAX_TRACKED_REQUESTS)
        );
        for index in 0..5_000 {
            state.remove_id(&format!("request-{index}"));
        }
        assert_eq!(pending_page_loads(&state, Instant::now()), Some(0));
        assert!(state.completed_before_start.0.is_empty());
    }

    #[test]
    fn an_oversize_url_and_id_are_tracked() {
        let mut state = NetworkQuietState::default();
        let id = "i".repeat(2 * MAX_REQUEST_ID_BYTES);
        fetch(
            &mut state,
            "long-url",
            format!(
                "https://example.test/?pad={}",
                "é".repeat(MAX_REQUEST_URL_BYTES)
            ),
        );
        fetch(&mut state, &id, "https://example.test/rows".into());
        assert_eq!(pending_page_loads(&state, Instant::now()), Some(2));
        assert!(state
            .requests()
            .all(|request| request.url.len() <= MAX_REQUEST_URL_BYTES));
        assert!(state
            .requests
            .keys()
            .all(|key| key.len() <= MAX_REQUEST_ID_BYTES));
        state.remove_id("long-url");
        state.remove_id(&id);
        assert_eq!(pending_page_loads(&state, Instant::now()), Some(0));
        assert!(state.completed_before_start.0.is_empty());
    }

    #[test]
    fn lost_tracking_is_unknown_until_the_top_document_commits() {
        let mut state = NetworkQuietState::default();
        assert!(state.mark_tracking_lost());
        assert!(!state.mark_tracking_lost());
        assert_eq!(pending_page_loads(&state, Instant::now()), None);
        assert!(counted_in_flight(&state, &NetworkQuietFilters::default(), Instant::now()).0 > 0);
        state.commit_document("child", "inner", false);
        assert_eq!(pending_page_loads(&state, Instant::now()), None);
        state.commit_document("main", "next", true);
        assert_eq!(pending_page_loads(&state, Instant::now()), Some(0));
    }
}
