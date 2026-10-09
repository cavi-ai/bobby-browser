//! Page-scoped in-flight tracking for Firefox `WaitCondition::NetworkQuiet`.
//!
//! Chromium owns a CDP tracker per page. Firefox multiplexes one BiDi event
//! stream, so this registry keys the shared [`NetworkQuietState`] machine by
//! browsing context and applies the same filter types.

use std::collections::HashMap;
use std::time::Instant;

use serde_json::Value;
use worker_pool::{
    counted_in_flight, map_bidi_network_type, pending_page_loads, NetworkQuietFilters,
    NetworkQuietState,
};

#[derive(Debug, Default)]
pub struct FirefoxNetworkQuiet {
    by_context: HashMap<String, NetworkQuietState>,
    unscoped: NetworkQuietState,
    owners: HashMap<String, Option<String>>,
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
            _ => {}
        }
    }

    pub fn drop_context(&mut self, context: &str) {
        self.by_context.remove(context);
        self.owners
            .retain(|_, owner| owner.as_deref() != Some(context));
    }

    pub fn snapshot(
        &self,
        context: &str,
        filters: &NetworkQuietFilters<'_>,
    ) -> (usize, Vec<String>) {
        let now = Instant::now();
        let (scoped_count, mut excluded) = self
            .by_context
            .get(context)
            .map(|state| counted_in_flight(state, filters, now))
            .unwrap_or((0, Vec::new()));
        let (unscoped_count, unscoped_excluded) = counted_in_flight(&self.unscoped, filters, now);
        for class in unscoped_excluded {
            if !excluded.contains(&class) {
                excluded.push(class);
            }
        }
        excluded.sort();
        (scoped_count + unscoped_count, excluded)
    }

    /// Script and fetch/XHR loads in flight for `context` (and unattributed ones).
    pub fn pending_page_loads(&self, context: &str) -> usize {
        let now = Instant::now();
        self.by_context
            .get(context)
            .map_or(0, |state| pending_page_loads(state, now))
            + pending_page_loads(&self.unscoped, now)
    }

    /// Script and fetch/XHR loads for `context` (and unattributed ones) that
    /// finished or failed.
    pub fn landed_page_loads(&self, context: &str) -> u64 {
        self.by_context
            .get(context)
            .map_or(0, NetworkQuietState::landed_page_loads)
            + self.unscoped.landed_page_loads()
    }

    pub fn mark_tracking_lost(&mut self) {
        self.unscoped.mark_tracking_lost();
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
        if (self.owners.len() >= 4096 && !self.owners.contains_key(&id))
            || id.len() > 1024
            || url.len() > 16 * 1024
            || (context
                .as_ref()
                .is_some_and(|context| !self.by_context.contains_key(context))
                && self.by_context.len() >= 256)
        {
            self.mark_tracking_lost();
            return;
        }
        self.owners.insert(id.clone(), context.clone());
        match context {
            Some(context) => {
                self.by_context
                    .entry(context)
                    .or_default()
                    .upsert_id(id, url, resource_type)
            }
            None => self.unscoped.upsert_id(id, url, resource_type),
        }
    }

    fn observe_finish(&mut self, id: &str) {
        match self.owners.remove(id) {
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
}

fn request_id(params: &Value) -> Option<String> {
    params
        .pointer("/request/request")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
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
        assert_eq!(quiet.landed_page_loads("tab"), 0);
        quiet.observe_event(
            "network.responseCompleted",
            &json!({"request": {"request": "r1"}}),
        );
        let (count, _) = quiet.snapshot("tab", &filters);
        assert_eq!(count, 0);
        assert_eq!(quiet.landed_page_loads("tab"), 1);
        assert_eq!(quiet.landed_page_loads("other"), 0);
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
    #[test]
    fn unknown_finishes_do_not_populate_every_context_and_destroy_releases_ownership() {
        let mut quiet = FirefoxNetworkQuiet::default();
        quiet.observe_event(
            "network.beforeRequestSent",
            &json!({"context":"tab", "request":{"request":"r1","url":"https://example.test"}}),
        );
        for index in 0..10_000 {
            quiet.observe_event(
                "network.responseCompleted",
                &json!({"request":{"request":format!("unknown-{index}")}}),
            );
        }
        assert_eq!(quiet.snapshot("tab", &NetworkQuietFilters::default()).0, 1);
        quiet.drop_context("tab");
        assert!(quiet.owners.is_empty());
        assert!(quiet.by_context.is_empty());
        assert_eq!(quiet.snapshot("tab", &NetworkQuietFilters::default()).0, 0);
    }
    #[test]
    fn event_loss_cannot_report_quiet_for_an_empty_or_new_context() {
        let mut quiet = FirefoxNetworkQuiet::default();
        quiet.mark_tracking_lost();
        assert!(quiet.snapshot("tab", &NetworkQuietFilters::default()).0 > 0);
    }
    #[test]
    fn global_request_ownership_is_bounded() {
        let mut quiet = FirefoxNetworkQuiet::default();
        for index in 0..10_000 {
            quiet.observe_event("network.beforeRequestSent", &json!({"context":"tab", "request":{"request":format!("r{index}"),"url":"https://example.test"}}));
        }
        assert!(quiet.owners.len() <= 4096);
        quiet.drop_context("tab");
        assert!(quiet.snapshot("new", &NetworkQuietFilters::default()).0 > 0);
    }
}
