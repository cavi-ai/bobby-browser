//! Page-scoped in-flight tracking for Firefox `WaitCondition::NetworkQuiet`.
//!
//! Chromium owns a CDP tracker per page. Firefox multiplexes one BiDi event
//! stream, so this registry keys the shared [`NetworkQuietState`] machine by
//! browsing context and applies the same filter types.

use std::collections::HashMap;
use std::time::Instant;

use serde_json::Value;
use worker_pool::{
    bounded_request_id, counted_in_flight, map_bidi_network_type, pending_page_loads,
    warn_tracking_lost, NetworkQuietFilters, NetworkQuietState, TrackingLoss, MAX_TRACKED_REQUESTS,
};

/// Browsing contexts tracked at once; past it an idle or the least recently
/// active one is dropped.
const MAX_CONTEXTS: usize = 256;

#[derive(Debug)]
struct Owner {
    context: Option<String>,
    /// Start order; the lowest is evicted first.
    order: u64,
}

#[derive(Debug, Default)]
pub struct FirefoxNetworkQuiet {
    by_context: HashMap<String, NetworkQuietState>,
    unscoped: NetworkQuietState,
    owners: HashMap<String, Owner>,
    starts: u64,
    /// The navigation each context started last and when, seen since the
    /// last event gap.
    navigations: HashMap<String, (String, Instant)>,
    /// After an event gap, a context without state of its own is unknown
    /// until it commits a document.
    gap: bool,
}

impl FirefoxNetworkQuiet {
    pub fn observe_event(&mut self, method: &str, params: &Value) {
        match method {
            "network.beforeRequestSent" => self.observe_start(params),
            "network.responseCompleted" | "network.fetchError" => {
                if let Some(id) = request_id(params) {
                    self.observe_finish(&id);
                }
            }
            "browsingContext.domContentLoaded" => self.observe_commit(params),
            _ => {}
        }
    }

    pub fn drop_context(&mut self, context: &str) {
        self.by_context.remove(context);
        self.navigations.remove(context);
        self.owners
            .retain(|_, owner| owner.context.as_deref() != Some(context));
    }

    pub fn snapshot(
        &self,
        context: &str,
        filters: &NetworkQuietFilters<'_>,
    ) -> (usize, Vec<String>) {
        let now = Instant::now();
        let (scoped_count, mut excluded) = match self.by_context.get(context) {
            Some(state) => counted_in_flight(state, filters, now),
            None if self.gap => (1, vec!["trackingLost".to_owned()]),
            None => (0, Vec::new()),
        };
        let (unscoped_count, unscoped_excluded) = counted_in_flight(&self.unscoped, filters, now);
        for class in unscoped_excluded {
            if !excluded.contains(&class) {
                excluded.push(class);
            }
        }
        excluded.sort();
        (scoped_count + unscoped_count, excluded)
    }

    /// Script and fetch/XHR loads in flight for `context` (and unattributed
    /// ones), `None` while an event gap leaves the context unknown.
    pub fn pending_page_loads(&self, context: &str) -> Option<usize> {
        let now = Instant::now();
        let scoped = match self.by_context.get(context) {
            Some(state) => pending_page_loads(state, now)?,
            None if self.gap => return None,
            None => 0,
        };
        Some(scoped + pending_page_loads(&self.unscoped, now)?)
    }

    /// Events were missed: every context is unknown until it commits a new
    /// document. Unattributed requests have no document, so they are dropped.
    pub fn mark_tracking_lost(&mut self, reason: TrackingLoss) {
        self.gap = true;
        self.navigations.clear();
        for state in self.by_context.values_mut() {
            state.mark_tracking_lost();
        }
        self.owners.retain(|_, owner| owner.context.is_some());
        self.unscoped = NetworkQuietState::default();
        warn_tracking_lost(reason);
    }

    fn observe_start(&mut self, params: &Value) {
        let Some(id) = request_id(params) else {
            return;
        };
        let url = params
            .pointer("/request/url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let resource_type = map_bidi_network_type(
            params
                .pointer("/request/destination")
                .and_then(Value::as_str),
            params
                .pointer("/request/initiatorType")
                .and_then(Value::as_str),
        );
        let context = params
            .get("context")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let now = Instant::now();
        if let (Some(context), Some(navigation)) = (
            context.as_deref(),
            params.get("navigation").and_then(Value::as_str),
        ) {
            self.observe_navigation(context, navigation, now);
        }
        if self.owners.len() >= MAX_TRACKED_REQUESTS && !self.owners.contains_key(&id) {
            self.evict_oldest_request();
        }
        self.starts += 1;
        self.owners.insert(
            id.clone(),
            Owner {
                context: context.clone(),
                order: self.starts,
            },
        );
        self.state_for(context.as_deref())
            .upsert_id(&id, url, resource_type);
    }

    fn observe_finish(&mut self, id: &str) {
        match self.owners.remove(id).map(|owner| owner.context) {
            Some(Some(context)) => {
                if let Some(state) = self.by_context.get_mut(&context) {
                    state.remove_id(id);
                }
            }
            Some(None) => self.unscoped.remove_id(id),
            // BiDi delivers a single ordered stream. A duplicate or late
            // finish for a destroyed context must not create tombstones in
            // every live context.
            None => {}
        }
    }

    fn observe_navigation(&mut self, context: &str, navigation: &str, now: Instant) {
        if self
            .navigations
            .get(context)
            .is_some_and(|(seen, _)| seen == navigation)
        {
            return;
        }
        if self.navigations.len() >= MAX_CONTEXTS && !self.navigations.contains_key(context) {
            let oldest = self
                .navigations
                .iter()
                .min_by_key(|(_, (_, started))| *started)
                .map(|(context, _)| context.clone());
            if let Some(oldest) = oldest {
                self.navigations.remove(&oldest);
            }
        }
        self.navigations
            .insert(context.to_owned(), (navigation.to_owned(), now));
    }

    /// The context's new document parsed: requests started before its
    /// navigation ended with the old document, and an event gap ends there.
    fn observe_commit(&mut self, params: &Value) {
        let (Some(context), Some(navigation)) = (
            params.get("context").and_then(Value::as_str),
            params.get("navigation").and_then(Value::as_str),
        ) else {
            return;
        };
        let started = match self.navigations.get(context) {
            Some((seen, started)) if seen == navigation => *started,
            _ => return,
        };
        self.navigations.remove(context);
        for id in self.state_for(Some(context)).commit_navigation(started) {
            self.owners.remove(&id);
        }
    }

    fn state_for(&mut self, context: Option<&str>) -> &mut NetworkQuietState {
        let Some(context) = context else {
            return &mut self.unscoped;
        };
        if self.by_context.len() >= MAX_CONTEXTS && !self.by_context.contains_key(context) {
            self.evict_context();
        }
        let gap = self.gap;
        self.by_context
            .entry(context.to_owned())
            .or_insert_with(|| {
                let mut state = NetworkQuietState::default();
                if gap {
                    state.mark_tracking_lost();
                }
                state
            })
    }

    fn evict_oldest_request(&mut self) {
        let oldest = self
            .owners
            .iter()
            .min_by_key(|(_, owner)| owner.order)
            .map(|(id, _)| id.clone());
        let Some((id, owner)) = oldest.and_then(|id| self.owners.remove_entry(&id)) else {
            return;
        };
        match owner.context {
            Some(context) => {
                if let Some(state) = self.by_context.get_mut(&context) {
                    state.drop_request(&id);
                }
            }
            None => self.unscoped.drop_request(&id),
        }
    }

    fn evict_context(&mut self) {
        let idle = self
            .by_context
            .iter()
            .find(|(_, state)| state.requests().next().is_none())
            .map(|(context, _)| context.clone());
        let victim = idle.or_else(|| {
            self.by_context
                .iter()
                .min_by_key(|(_, state)| state.requests().map(|request| request.started_at).max())
                .map(|(context, _)| context.clone())
        });
        if let Some(context) = victim {
            self.drop_context(&context);
        }
    }
}

fn request_id(params: &Value) -> Option<String> {
    params
        .pointer("/request/request")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(bounded_request_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use types::NetworkResourceType;

    #[test]
    fn in_flight_fetch_blocks_until_response_completed() {
        let mut quiet = FirefoxNetworkQuiet::default();
        quiet.observe_event(
            "network.beforeRequestSent",
            &json!({
                "context": "tab",
                "request": {
                    "request": "r1",
                    "url": "https://example.test/api",
                    "destination": "empty",
                    "initiatorType": "fetch"
                }
            }),
        );
        let filters = NetworkQuietFilters::default();
        let (count, _) = quiet.snapshot("tab", &filters);
        assert_eq!(count, 1);
        quiet.observe_event(
            "network.responseCompleted",
            &json!({"request": {"request": "r1"}}),
        );
        let (count, _) = quiet.snapshot("tab", &filters);
        assert_eq!(count, 0);
    }

    #[test]
    fn url_substring_filter_excludes_analytics() {
        let mut quiet = FirefoxNetworkQuiet::default();
        quiet.observe_event(
            "network.beforeRequestSent",
            &json!({
                "context": "tab",
                "request": {
                    "request": "r1",
                    "url": "https://cdn.example.test/analytics.js",
                    "destination": "script"
                }
            }),
        );
        let ignore = vec!["analytics".to_owned()];
        let filters = NetworkQuietFilters {
            ignore_url_substrings: &ignore,
            ignore_resource_types: &[],
            ignore_long_lived: false,
        };
        let (count, excluded) = quiet.snapshot("tab", &filters);
        assert_eq!(count, 0);
        assert_eq!(excluded, vec!["urlSubstring:analytics".to_owned()]);
        assert_eq!(
            map_bidi_network_type(Some("script"), None),
            NetworkResourceType::Script
        );
    }
}

#[cfg(test)]
mod retention_tests {
    use super::*;
    use serde_json::json;

    fn fetch(quiet: &mut FirefoxNetworkQuiet, context: &str, id: &str, url: &str) {
        quiet.observe_event(
            "network.beforeRequestSent",
            &json!({"context": context, "navigation": null,
                    "request": {"request": id, "url": url, "initiatorType": "fetch"}}),
        );
    }

    fn navigate(quiet: &mut FirefoxNetworkQuiet, context: &str, id: &str, navigation: &str) {
        quiet.observe_event(
            "network.beforeRequestSent",
            &json!({"context": context, "navigation": navigation,
                    "request": {"request": id, "url": "https://example.test/next",
                                "destination": "document"}}),
        );
    }

    fn finish(quiet: &mut FirefoxNetworkQuiet, id: &str) {
        quiet.observe_event(
            "network.responseCompleted",
            &json!({"request": {"request": id}}),
        );
    }

    fn commit(quiet: &mut FirefoxNetworkQuiet, context: &str, navigation: &str) {
        quiet.observe_event(
            "browsingContext.domContentLoaded",
            &json!({"context": context, "navigation": navigation, "url": "https://example.test/next"}),
        );
    }

    #[test]
    fn unknown_finishes_do_not_populate_every_context_and_destroy_releases_ownership() {
        let mut quiet = FirefoxNetworkQuiet::default();
        quiet.observe_event(
            "network.beforeRequestSent",
            &json!({"context":"tab", "request":{"request":"r1","url":"https://example.test"}}),
        );
        for index in 0..10_000 {
            finish(&mut quiet, &format!("unknown-{index}"));
        }
        assert_eq!(quiet.snapshot("tab", &NetworkQuietFilters::default()).0, 1);
        assert_eq!(quiet.pending_page_loads("tab"), Some(0));
        quiet.drop_context("tab");
        assert!(quiet.owners.is_empty());
        assert!(quiet.by_context.is_empty());
        assert_eq!(quiet.snapshot("tab", &NetworkQuietFilters::default()).0, 0);
    }

    #[test]
    fn capacity_evicts_the_oldest_request_and_keeps_counting() {
        let mut quiet = FirefoxNetworkQuiet::default();
        for index in 0..5_000 {
            fetch(
                &mut quiet,
                "tab",
                &format!("r{index}"),
                "https://example.test/item",
            );
        }
        assert_eq!(quiet.owners.len(), MAX_TRACKED_REQUESTS);
        assert_eq!(quiet.pending_page_loads("tab"), Some(MAX_TRACKED_REQUESTS));
        finish(&mut quiet, "r0");
        assert_eq!(quiet.pending_page_loads("tab"), Some(MAX_TRACKED_REQUESTS));
        for index in 904..5_000 {
            finish(&mut quiet, &format!("r{index}"));
        }
        assert_eq!(quiet.pending_page_loads("tab"), Some(0));
        assert_eq!(quiet.snapshot("tab", &NetworkQuietFilters::default()).0, 0);
    }

    #[test]
    fn an_oversize_url_and_id_are_tracked() {
        let mut quiet = FirefoxNetworkQuiet::default();
        let url = format!("https://example.test/item?pad={}", "a".repeat(20 * 1024));
        let id = "i".repeat(2048);
        fetch(&mut quiet, "tab", "long-url", &url);
        fetch(&mut quiet, "tab", &id, "https://example.test/item");
        assert_eq!(quiet.pending_page_loads("tab"), Some(2));
        let tracked = &quiet.by_context["tab"];
        assert!(tracked
            .requests()
            .all(|request| request.url.len() <= 16 * 1024));
        finish(&mut quiet, "long-url");
        finish(&mut quiet, &id);
        assert_eq!(quiet.pending_page_loads("tab"), Some(0));
    }

    #[test]
    fn contexts_past_the_bound_drop_an_idle_one() {
        let mut quiet = FirefoxNetworkQuiet::default();
        fetch(&mut quiet, "busy", "r-busy", "https://example.test/item");
        for index in 0..300 {
            let id = format!("r{index}");
            fetch(
                &mut quiet,
                &format!("frame-{index}"),
                &id,
                "https://example.test/item",
            );
            finish(&mut quiet, &id);
        }
        assert!(quiet.by_context.len() <= MAX_CONTEXTS);
        assert_eq!(quiet.pending_page_loads("busy"), Some(1));
        fetch(&mut quiet, "fresh", "r-fresh", "https://example.test/item");
        assert_eq!(quiet.pending_page_loads("fresh"), Some(1));
    }

    #[test]
    fn a_commit_ends_the_old_documents_requests() {
        let mut quiet = FirefoxNetworkQuiet::default();
        fetch(&mut quiet, "tab", "old", "https://example.test/poll");
        navigate(&mut quiet, "tab", "doc", "nav-1");
        commit(&mut quiet, "tab", "nav-other");
        assert_eq!(quiet.pending_page_loads("tab"), Some(1));
        commit(&mut quiet, "tab", "nav-1");
        assert_eq!(quiet.pending_page_loads("tab"), Some(0));
        fetch(&mut quiet, "tab", "rows", "https://example.test/rows");
        finish(&mut quiet, "old");
        assert_eq!(quiet.pending_page_loads("tab"), Some(1));
    }

    #[test]
    fn an_event_gap_is_unknown_per_context_until_it_commits() {
        let mut quiet = FirefoxNetworkQuiet::default();
        fetch(&mut quiet, "tab", "before", "https://example.test/poll");
        fetch(
            &mut quiet,
            "other",
            "elsewhere",
            "https://example.test/poll",
        );
        navigate(&mut quiet, "tab", "doc-early", "nav-early");
        quiet.mark_tracking_lost(TrackingLoss::Lagged);
        assert_eq!(quiet.pending_page_loads("tab"), None);
        assert!(quiet.snapshot("tab", &NetworkQuietFilters::default()).0 > 0);
        assert_eq!(quiet.pending_page_loads("new"), None);
        assert!(quiet.snapshot("new", &NetworkQuietFilters::default()).0 > 0);
        commit(&mut quiet, "tab", "nav-early");
        assert_eq!(quiet.pending_page_loads("tab"), None);

        navigate(&mut quiet, "tab", "doc", "nav-1");
        fetch(&mut quiet, "tab", "rows", "https://example.test/rows");
        commit(&mut quiet, "tab", "nav-1");
        assert_eq!(quiet.pending_page_loads("tab"), Some(1));
        assert_eq!(quiet.pending_page_loads("other"), None);
        finish(&mut quiet, "rows");
        finish(&mut quiet, "doc");
        assert_eq!(quiet.pending_page_loads("tab"), Some(0));
        assert_eq!(quiet.snapshot("tab", &NetworkQuietFilters::default()).0, 0);
    }
}
