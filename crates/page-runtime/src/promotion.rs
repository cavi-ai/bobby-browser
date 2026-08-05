//! Write path for the durable context graph (Spec C): promotes verified
//! intent outcomes from the session-hot [`ContextGraph`] layer into the
//! per-profile [`ContextStore`].
//!
//! Attached to a [`PageRuntime`] only when the runtime's engine selection
//! carries a durable profile identity (Firefox companion). Chromium sessions
//! run with no promotion handle at all: disposable profiles have no durable
//! identity to key by, so they read nothing and write nothing.
//!
//! Only structure is promoted: the resolved control's role, accessible name,
//! and ordinal; the intent kind; counters; a coarse day-precision timestamp.
//! Typed values, credentials, page text, and exact timestamps never leave
//! the session. Writes are buffered in memory and flushed on session close;
//! a persistence failure degrades to session-only with one log event and
//! never fails the command that triggered it.

use std::collections::BTreeMap;

use context_store::{
    day_since_epoch, site_key, ContextStore, ControlContext, IntentStats, RecordSource,
};
use types::{Evidence, IntentResolutionPath, TargetSpec};

/// Form key used until form membership is observed structurally. Schema v1
/// keeps every promoted control in one page-level form rather than guessing
/// membership from incomplete evidence.
const PAGE_LEVEL_FORM: &str = "page";

pub struct ContextPromotion {
    store: ContextStore,
}

impl ContextPromotion {
    pub fn new(store: ContextStore) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &ContextStore {
        &self.store
    }

    /// Promotes the resolved target of a completed or failed command.
    ///
    /// `evidence` is the command's evidence; a `Resolution` item pins the
    /// exact target that was acted on, an `IntentExecution` record supplies
    /// the intent kind, the resolution path, and (on failure, where no
    /// resolution was reached) the best candidate seen.
    pub async fn record_outcome(
        &self,
        page_url: Option<&str>,
        evidence: &[Evidence],
        success: bool,
    ) {
        let Some(url) = page_url else { return };
        let Some(site) = site_key(url) else { return };
        let Some(pattern) = page_pattern(url) else {
            return;
        };
        let record = evidence.iter().find_map(|item| match item {
            Evidence::IntentExecution { record } => Some(record),
            _ => None,
        });
        let Some(record) = record else { return };
        let resolution = evidence.iter().find_map(|item| match item {
            Evidence::Resolution { target, .. } => Some(target.as_ref()),
            _ => None,
        });
        let control = match resolution {
            Some(target) => control_from_target(target),
            None if !success => record.candidates.first().and_then(|candidate| {
                Some(ControlContext {
                    role: candidate.role.clone()?,
                    accessible_name: candidate.name.clone()?,
                    ordinal: None,
                    form_membership: PAGE_LEVEL_FORM.to_string(),
                    intents: BTreeMap::new(),
                })
            }),
            None => None,
        };
        let Some(mut control) = control else { return };
        let source = match record.resolution_path {
            IntentResolutionPath::VisionFallback => RecordSource::VisionPromoted,
            IntentResolutionPath::Deterministic => RecordSource::Observed,
        };
        let stats = control
            .intents
            .entry(record.intent_kind.clone())
            .or_default();
        apply_outcome(stats, success, source);

        let mut site_context = self.store.site(&site).await.unwrap_or_default();
        let page = site_context.pages.entry(pattern).or_default();
        let form = page.forms.entry(PAGE_LEVEL_FORM.to_string()).or_default();
        match form.controls.iter_mut().find(|existing| {
            existing.role == control.role
                && existing.accessible_name == control.accessible_name
                && existing.ordinal == control.ordinal
        }) {
            Some(existing) => {
                let stats = existing
                    .intents
                    .entry(record.intent_kind.clone())
                    .or_default();
                apply_outcome(stats, success, source);
            }
            None => form.controls.push(control),
        }
        self.store.upsert_site(&site, site_context).await;
    }

    /// Flushes buffered writes; failures stay session-only (the store keeps
    /// them dirty) and are reported once here, never to the command path.
    pub async fn flush(&self) {
        let failed = self.store.flush().await;
        if !failed.is_empty() {
            tracing::warn!(
                sites = ?failed,
                "context.promotion_degraded"
            );
        }
    }
}

fn apply_outcome(stats: &mut IntentStats, success: bool, source: RecordSource) {
    if success {
        stats.success_count += 1;
        stats.last_verified_day = Some(day_since_epoch(chrono::Utc::now()));
        stats.source = Some(source);
    } else {
        stats.failure_count += 1;
    }
}

fn control_from_target(target: &TargetSpec) -> Option<ControlContext> {
    Some(ControlContext {
        role: target.role.clone()?,
        accessible_name: target.accessible_name.clone()?,
        ordinal: target.ordinal.map(|ordinal| ordinal as u32),
        form_membership: PAGE_LEVEL_FORM.to_string(),
        intents: BTreeMap::new(),
    })
}

/// Page pattern for persistence: path only, query and fragment stripped,
/// segments carrying digits templated to `{}` so per-entity URLs share one
/// pattern. Never the full URL.
fn page_pattern(page_url: &str) -> Option<String> {
    let parsed = url::Url::parse(page_url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let pattern: Vec<String> = parsed
        .path()
        .split('/')
        .map(|segment| {
            if templated(segment) {
                "{}".to_string()
            } else {
                segment.to_string()
            }
        })
        .collect();
    let joined = pattern.join("/");
    Some(if joined.is_empty() {
        "/".to_string()
    } else {
        joined
    })
}

fn templated(segment: &str) -> bool {
    if segment.is_empty() {
        return false;
    }
    if segment.chars().all(|character| character.is_ascii_digit()) {
        return true;
    }
    segment.len() > 2 && segment.chars().any(|character| character.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::page_pattern;

    #[test]
    fn page_patterns_strip_query_and_template_parameters() {
        let cases: &[(&str, Option<&str>)] = &[
            ("https://example.com/login?next=/home#form", Some("/login")),
            ("https://example.com/", Some("/")),
            ("https://example.com", Some("/")),
            (
                "https://example.com/customers/cus_1042",
                Some("/customers/{}"),
            ),
            (
                "https://example.com/customers/42/orders/7",
                Some("/customers/{}/orders/{}"),
            ),
            ("https://example.com/a/b", Some("/a/b")),
            ("about:blank", None),
        ];
        for (input, expected) in cases {
            assert_eq!(page_pattern(input).as_deref(), *expected, "input: {input}");
        }
    }
}
