//! Page-scoped in-flight tracking for Firefox `WaitCondition::NetworkQuiet`.
//!
//! Chromium owns a CDP tracker per page. Firefox multiplexes one BiDi event
//! stream, so this registry keys the shared [`NetworkQuietState`] machine by
//! browsing context and applies the same filter types.

use std::collections::HashMap;
use std::time::Instant;

use serde_json::Value;
use worker_pool::{
    counted_in_flight, map_bidi_network_type, NetworkQuietFilters, NetworkQuietState,
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
            None => {
                self.unscoped.remove_id(id);
                for state in self.by_context.values_mut() {
                    state.remove_id(id);
                }
            }
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
