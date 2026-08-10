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
use types::{ContextAnswer, ContextAnswerSource, Evidence, IntentResolutionPath, TargetSpec};

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
            IntentResolutionPath::VisionFallback | IntentResolutionPath::VisionPrefill => {
                RecordSource::VisionPromoted
            }
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

    /// Answers a control question from the persisted store alone — the
    /// cold-start path. The same matching ladder as the hot graph (exact
    /// 1.0, role+name 0.9, token-overlap fuzzy ≤0.8, floor and tie-refusal
    /// shared) runs over remembered controls. Answers are marked
    /// `Persisted` with the record's source; unknown sites answer `None`,
    /// exactly like an unobserved page.
    pub async fn ask(&self, page_url: Option<&str>, description: &str) -> Option<ContextAnswer> {
        let url = page_url?;
        let site = site_key(url)?;
        let needle = description.trim().to_lowercase();
        if needle.is_empty() {
            return None;
        }
        let site_context = self.store.site(&site).await?;
        // Tie-refusal makes iteration order irrelevant: two same-score
        // controls answer `None` wherever they were remembered.
        let controls = site_context
            .pages
            .values()
            .flat_map(|page| page.forms.values())
            .flat_map(|form| form.controls.iter());
        let mut best: Option<(f32, &context_store::ControlContext)> = None;
        let mut tied = false;
        for control in controls {
            let Some(score) = score_remembered(&needle, control) else {
                continue;
            };
            match &best {
                Some((current, _)) if *current > score => {}
                Some((current, _)) if (*current - score).abs() < f32::EPSILON => tied = true,
                _ => {
                    best = Some((score, control));
                    tied = false;
                }
            }
        }
        let (confidence, control) = best?;
        if tied || confidence < crate::CONTEXT_CONFIDENCE_FLOOR {
            return None;
        }
        let source = control
            .intents
            .values()
            .filter_map(|stats| stats.source)
            .next()
            .map(|source| match source {
                RecordSource::Observed => ContextAnswerSource::Observed,
                RecordSource::VisionPromoted => ContextAnswerSource::VisionPromoted,
            });
        Some(ContextAnswer {
            target: types::AccessibilityTarget {
                role: control.role.clone(),
                accessible_name: control.accessible_name.clone(),
                ordinal: control.ordinal.map(|ordinal| ordinal as usize),
                frame_path: Vec::new(),
            },
            confidence,
            observed_at: types::ContextObservedAt::Persisted,
            source,
        })
    }

    /// The remembered structure of one whole site, or `None` when the
    /// store has never seen it.
    pub async fn site_view(&self, site: &str) -> Option<types::ContextSiteView> {
        let site_context = self.store.site(site).await?;
        Some(types::ContextSiteView {
            site_key: site.to_string(),
            pages: site_context
                .pages
                .iter()
                .map(|(pattern, page)| {
                    (
                        pattern.clone(),
                        page.forms
                            .iter()
                            .map(|(form, context)| {
                                (
                                    form.clone(),
                                    context.controls.iter().map(control_view).collect(),
                                )
                            })
                            .collect(),
                    )
                })
                .collect(),
        })
    }

    /// The remembered form structure around a located control: the answer
    /// plus the enclosing form's controls with their per-intent counters.
    /// `None` when the store cannot locate the control, exactly like `ask`.
    pub async fn neighbors(
        &self,
        page_url: Option<&str>,
        description: &str,
    ) -> Option<types::ContextNeighbors> {
        let url = page_url?;
        let site = site_key(url)?;
        let answer = self.ask(page_url, description).await?;
        let site_context = self.store.site(&site).await?;
        for (pattern, page) in &site_context.pages {
            for (form_key, form) in &page.forms {
                if !form.controls.iter().any(|control| {
                    control.role == answer.target.role
                        && control.accessible_name == answer.target.accessible_name
                        && control.ordinal.map(|ordinal| ordinal as usize) == answer.target.ordinal
                }) {
                    continue;
                }
                return Some(types::ContextNeighbors {
                    answer,
                    form: form_key.clone(),
                    page_pattern: pattern.clone(),
                    controls: form.controls.iter().map(control_view).collect(),
                });
            }
        }
        None
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

fn control_view(control: &context_store::ControlContext) -> types::ContextNeighborControl {
    types::ContextNeighborControl {
        role: control.role.clone(),
        accessible_name: control.accessible_name.clone(),
        ordinal: control.ordinal.map(|ordinal| ordinal as usize),
        intents: control
            .intents
            .iter()
            .map(|(kind, stats)| {
                (
                    kind.clone(),
                    types::ContextNeighborStats {
                        success_count: stats.success_count,
                        failure_count: stats.failure_count,
                        last_verified_day: stats.last_verified_day,
                        source: stats.source.map(|source| match source {
                            RecordSource::Observed => ContextAnswerSource::Observed,
                            RecordSource::VisionPromoted => ContextAnswerSource::VisionPromoted,
                        }),
                    },
                )
            })
            .collect(),
    }
}

fn score_remembered(needle: &str, control: &context_store::ControlContext) -> Option<f32> {
    let name = control.accessible_name.trim().to_lowercase();
    if name == needle {
        return Some(1.0);
    }
    let role = control.role.trim().to_lowercase();
    if format!("{role} {name}") == needle {
        return Some(0.9);
    }
    crate::context::fuzzy_score(needle, &name)
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
