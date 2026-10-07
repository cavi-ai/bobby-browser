//! Write path for the durable context graph (Spec C): promotes verified
//! intent outcomes from the session-hot [`ContextGraph`] layer into the
//! per-profile [`ContextStore`].
//!
//! Attached to a [`PageRuntime`] only when the runtime's engine selection
//! carries a durable profile identity: an enrolled Firefox companion, a named
//! Chromium profile, or managed Chromium's shared `managed-chromium` identity.
//!
//! Only structure is promoted: the resolved control's role, accessible name,
//! and ordinal; the intent kind; counters; a coarse day-precision timestamp.
//! Typed values, credentials, page text, and exact timestamps never leave
//! the session. Each promoted outcome is written to disk before the command
//! returns; a persistence failure degrades to session-only with one log
//! event, is retried at the next flush, and never fails the command that
//! triggered it.

use std::collections::BTreeMap;
use std::sync::Arc;

use context_store::{
    day_since_epoch, page_pattern, site_key, ContextStore, ControlContext, IntentStats,
    RecordSource,
};
use types::{ContextAnswer, ContextAnswerSource, Evidence, IntentResolutionPath, TargetSpec};

/// Form key used until form membership is observed structurally. Schema v1
/// keeps every promoted control in one page-level form rather than guessing
/// membership from incomplete evidence.
const PAGE_LEVEL_FORM: &str = "page";

pub struct ContextPromotion {
    store: Arc<ContextStore>,
}

impl ContextPromotion {
    pub fn new(store: ContextStore) -> Self {
        Self {
            store: Arc::new(store),
        }
    }

    pub fn store(&self) -> &ContextStore {
        &self.store
    }

    /// Shared handle to the store, for read-side consumers (challenge
    /// priors in the intent engine) that must not own the promotion sink.
    pub fn store_arc(&self) -> Arc<ContextStore> {
        self.store.clone()
    }

    /// Promotes the resolved targets of a completed or failed command and
    /// writes them to disk before returning.
    ///
    /// `evidence` is the command's evidence. Each `IntentExecution` record
    /// supplies an intent kind and resolution path for the `Resolution`
    /// items emitted since the previous record: one for a fill, one per
    /// field for a completed form or an extraction. A completed command
    /// promotes every such group; a failed one promotes only its last group,
    /// the step that failed, falling back to that record's best candidate
    /// when no resolution was reached.
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
        let mut groups = Vec::new();
        let mut resolved = Vec::new();
        for item in evidence {
            match item {
                Evidence::Resolution { target, .. } => resolved.push(target.as_ref()),
                Evidence::IntentExecution { record } => {
                    groups.push((record, std::mem::take(&mut resolved)));
                }
                _ => {}
            }
        }
        if !success {
            groups = groups.split_off(groups.len().saturating_sub(1));
        }
        if groups.is_empty() {
            return;
        }

        let mut controls = Vec::new();
        for (record, targets) in groups {
            // A challenge solve has no resolved control — it acts on pixels
            // inside a widget iframe. Record it at site level as the challenge
            // prior instead of forcing it into the control schema.
            if record.intent_kind == "solveChallenge" {
                self.store
                    .record_challenge(
                        &site,
                        "solveChallenge",
                        success,
                        day_since_epoch(chrono::Utc::now()),
                    )
                    .await;
                continue;
            }
            let source = match record.resolution_path {
                IntentResolutionPath::VisionFallback | IntentResolutionPath::VisionPrefill => {
                    RecordSource::VisionPromoted
                }
                IntentResolutionPath::Deterministic => RecordSource::Observed,
            };
            if targets.is_empty() && !success {
                if let Some(control) = record.candidates.first().and_then(|candidate| {
                    Some(ControlContext {
                        role: candidate.role.clone()?,
                        accessible_name: candidate.name.clone()?,
                        ordinal: None,
                        form_membership: PAGE_LEVEL_FORM.to_string(),
                        intents: BTreeMap::new(),
                    })
                }) {
                    controls.push((control, record.intent_kind.clone(), source));
                }
            }
            for target in targets {
                if let Some(control) = control_from_target(target) {
                    controls.push((control, record.intent_kind.clone(), source));
                }
            }
        }

        if !controls.is_empty() {
            self.store
                .update_site(&site, |site_context| {
                    let page = site_context.pages.entry(pattern).or_default();
                    let form = page.forms.entry(PAGE_LEVEL_FORM.to_string()).or_default();
                    for (control, intent_kind, source) in controls {
                        let index = match form.controls.iter().position(|existing| {
                            existing.role == control.role
                                && existing.accessible_name == control.accessible_name
                                && existing.ordinal == control.ordinal
                        }) {
                            Some(index) => index,
                            None => {
                                form.controls.push(control);
                                form.controls.len() - 1
                            }
                        };
                        let stats = form.controls[index].intents.entry(intent_kind).or_default();
                        apply_outcome(stats, success, source);
                    }
                })
                .await;
        }
        // A runtime can stop without any session closing (a shared owner
        // restart, a killed process), so the outcome is written now rather
        // than at session close.
        self.flush().await;
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
        // Ladder first, validation record second: the name-match ladder
        // (exact 1.0, role+name 0.9, fuzzy ≤0.8) decides which controls match
        // best; the validation weight only breaks ties between equal matches.
        // That keeps the pinned ladder semantics — a fuzzy match never beats
        // an exact one — while more-verified and more-recent entries win the
        // match they tied on. Full ties (same match, same record) still
        // refuse, so iteration order stays irrelevant.
        let today = day_since_epoch(chrono::Utc::now());
        let controls = site_context
            .pages
            .values()
            .flat_map(|page| page.forms.values())
            .flat_map(|form| form.controls.iter());
        let mut best: Option<(f32, f32, &context_store::ControlContext)> = None;
        let mut tied = false;
        for control in controls {
            let Some(score) = score_remembered(&needle, control) else {
                continue;
            };
            let weight = validation_weight(control, today);
            match &best {
                Some((current, current_weight, _)) if *current > score => {}
                Some((current, current_weight, _)) if (*current - score).abs() < f32::EPSILON => {
                    if (*current_weight - weight).abs() < f32::EPSILON {
                        tied = true;
                    } else if weight > *current_weight {
                        best = Some((score, weight, control));
                        tied = false;
                    }
                }
                _ => {
                    best = Some((score, weight, control));
                    tied = false;
                }
            }
        }
        let (confidence, _, control) = best?;
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

/// Recency-frequency weight for a remembered control, used to break ladder
/// ties: entries that verified more often and more recently outrank stale or
/// failing ones. Aggregates the control's per-intent stats (the recall
/// question carries no intent kind). Validation count boosts with
/// diminishing returns; failures drag down; verification recency decays on
/// a 30-day half-life with a floor at half weight so old-but-good entries
/// still compete.
fn validation_weight(control: &context_store::ControlContext, today: u32) -> f32 {
    let mut successes: u64 = 0;
    let mut failures: u64 = 0;
    let mut newest_day: Option<u32> = None;
    for stats in control.intents.values() {
        successes += stats.success_count;
        failures += stats.failure_count;
        if let Some(day) = stats.last_verified_day {
            newest_day = Some(newest_day.map_or(day, |newest| newest.max(day)));
        }
    }
    let total = successes + failures;
    if total == 0 {
        return 1.0;
    }
    let validation_boost = 1.0 + (successes as f32).ln_1p();
    let reliability = successes as f32 / (total as f32 + 1.0);
    let recency = match newest_day {
        Some(day) => 0.5_f32.powi(today.saturating_sub(day) as i32 / 30),
        None => 0.5,
    };
    validation_boost * reliability * (0.5 + 0.5 * recency)
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

#[cfg(test)]
mod tests {
    use super::page_pattern;
    use context_store::{ControlContext, IntentStats};
    use std::collections::BTreeMap;

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

    fn control(name: &str, successes: u64, failures: u64, last_day: Option<u32>) -> ControlContext {
        let mut intents = BTreeMap::new();
        intents.insert(
            "locate".to_string(),
            IntentStats {
                success_count: successes,
                failure_count: failures,
                last_verified_day: last_day,
                source: None,
            },
        );
        ControlContext {
            role: "button".into(),
            accessible_name: name.into(),
            ordinal: None,
            form_membership: "page".into(),
            intents,
        }
    }

    #[test]
    fn more_validated_entries_carry_more_weight() {
        let today = 20_000;
        let fresh = control("Save", 5, 0, Some(today));
        let thin = control("Save", 0, 0, None);
        let fresh_weight = super::validation_weight(&fresh, today);
        let thin_weight = super::validation_weight(&thin, today);
        assert!(
            fresh_weight > thin_weight,
            "validated {fresh_weight} must beat unvalidated {thin_weight}"
        );
    }

    #[test]
    fn recently_verified_entries_carry_more_weight() {
        let today = 20_000;
        let recent = control("Save", 3, 0, Some(today));
        let stale = control("Save", 3, 0, Some(today - 120));
        let recent_weight = super::validation_weight(&recent, today);
        let stale_weight = super::validation_weight(&stale, today);
        assert!(
            recent_weight > stale_weight,
            "recent {recent_weight} must beat stale {stale_weight}"
        );
    }

    #[test]
    fn failures_drag_the_weight_down() {
        let today = 20_000;
        let clean = control("Save", 4, 0, Some(today));
        let flaky = control("Save", 4, 4, Some(today));
        let clean_weight = super::validation_weight(&clean, today);
        let flaky_weight = super::validation_weight(&flaky, today);
        assert!(
            clean_weight > flaky_weight,
            "clean {clean_weight} must beat flaky {flaky_weight}"
        );
    }

    #[test]
    fn unrecorded_controls_keep_the_plain_match_score() {
        // No stats: the weight is exactly 1.0 and the ladder score is
        // untouched, so the pinned match ladder (1.0 / 0.9 / fuzzy) is
        // preserved for controls with no validation record.
        let today = 20_000;
        let bare = control("Save", 0, 0, None);
        assert_eq!(super::validation_weight(&bare, today), 1.0);
        let score = super::score_remembered("save", &bare).unwrap();
        assert_eq!(score, 1.0);
    }
}
