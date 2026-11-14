//! Verifies a submit the caller gave no expected state: the page changes
//! URL, the submit control leaves it, or the form shows other fields. Fields
//! the page marks invalid while it stays put are a rejection.

use std::time::Duration;

use super::*;

/// Longest a page may take to change after a submit with no expected state.
const PAGE_CHANGE_MS: u64 = 10_000;
/// How long each poll watches the submit control; also the poll cadence.
const CONTROL_POLL_MS: u64 = 250;
/// A rejection is read only after this long, and only on a quiet network, so
/// a submit still in flight is not taken for one.
const REJECTION_AFTER: Duration = Duration::from_secs(1);
/// Continuous network quiet that settles a change before it is trusted.
const NETWORK_QUIET_MS: u64 = 300;
/// Longest a seen change waits for the network to settle.
const SETTLE_MS: u64 = 2_000;

/// The page as it was before the submit.
#[derive(Default)]
pub(super) struct Baseline {
    url: Option<String>,
    fields: Option<Vec<submit_settlement::Field>>,
}

pub(super) enum PageChange {
    /// The page moved on; `evidence` says how.
    Changed {
        evidence: Vec<Evidence>,
        verification: &'static str,
    },
    /// The page stayed and marks these fields invalid.
    Rejected {
        issues: Vec<types::FormValidationIssue>,
    },
}

pub(super) async fn baseline(page_id: &PageId, browser: &dyn IntentBrowser) -> Baseline {
    Baseline {
        url: read_url(page_id, browser).await,
        fields: form_fields(page_id, browser).await,
    }
}

/// Polls until the page changes, rejects the submit, or the window ends.
/// `control` is the submit control as it was clicked.
pub(super) async fn await_change(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    control: &TargetSpec,
    before: &Baseline,
    command_deadline: Option<tokio::time::Instant>,
) -> Result<PageChange, CommandError> {
    let started = tokio::time::Instant::now();
    let mut deadline = started + Duration::from_millis(PAGE_CHANGE_MS);
    if let Some(command_deadline) = command_deadline {
        // Leaves the post-submit reading its time.
        let reserve = OUTCOME_READING_MIN + Duration::from_secs(1);
        deadline = deadline.min(command_deadline.checked_sub(reserve).unwrap_or(started));
    }
    let mut observations = 0;
    let mut saw_same_fields = false;
    loop {
        observations += 1;
        // A seen change settles first; a navigation it was part of wins.
        if let Some(changed) = url_changed(page_id, browser, before, started, observations).await {
            return Ok(changed);
        }
        if control_left(page_id, browser, control).await.is_some() {
            network_quiet(page_id, browser, SETTLE_MS).await;
            if let Some(changed) =
                url_changed(page_id, browser, before, started, observations).await
            {
                return Ok(changed);
            }
            if let Some(evidence) = control_left(page_id, browser, control).await {
                return Ok(PageChange::Changed {
                    evidence,
                    verification: "controlLeft",
                });
            }
        }
        let mut settlement = read_settlement(page_id, browser, before).await;
        if changed_fields(page_id, browser, &settlement)
            .await
            .is_some()
        {
            network_quiet(page_id, browser, SETTLE_MS).await;
            if let Some(changed) =
                url_changed(page_id, browser, before, started, observations).await
            {
                return Ok(changed);
            }
            settlement = read_settlement(page_id, browser, before).await;
            if let Some(evidence) = changed_fields(page_id, browser, &settlement).await {
                return Ok(PageChange::Changed {
                    evidence,
                    verification: "formChanged",
                });
            }
        }
        if same_fields(&settlement) {
            saw_same_fields = true;
        }
        let now = tokio::time::Instant::now();
        let last = now >= deadline;
        if now.duration_since(started) >= REJECTION_AFTER
            && (last || network_quiet(page_id, browser, CONTROL_POLL_MS * 2).await)
        {
            if let submit_settlement::Observation::Looked(issues) = settlement.issues {
                if !issues.is_empty() {
                    return Ok(PageChange::Rejected { issues });
                }
            }
        }
        if last {
            let fields = if saw_same_fields {
                "the same form fields"
            } else {
                "the form was not read"
            };
            return Err(CommandError {
                code: ErrorCode::WaitConditionTimedOut,
                message: format!(
                    "the page did not change within {PAGE_CHANGE_MS}ms: same URL, the submit \
                     control still shown, {fields}. A page that confirms in place \
                     needs an expectedState"
                ),
                layer: ErrorLayer::Page,
                retryable: false,
            });
        }
    }
}

/// Names the invalid fields of a rejected submit, with the page's message.
pub(super) fn describe_rejection(issues: &[types::FormValidationIssue]) -> String {
    issues
        .iter()
        .take(8)
        .map(|issue| {
            let name = issue
                .accessible_name
                .as_deref()
                .unwrap_or(&issue.control_id);
            match issue.validity.message.as_deref() {
                Some(message) if !message.is_empty() => format!("\"{name}\" ({message})"),
                _ => format!("\"{name}\""),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

async fn read_url(page_id: &PageId, browser: &dyn IntentBrowser) -> Option<String> {
    let probe = WaitForCommand {
        condition: WaitCondition::Url {
            matcher: types::TextMatch::Contains(String::new()),
        },
        timeout_ms: 1_000,
    };
    browser
        .wait_for(page_id, &probe)
        .await
        .ok()?
        .into_iter()
        .find_map(|item| match item {
            Evidence::Wait { observed, .. } => observed,
            _ => None,
        })
}

/// The change to a URL other than the baseline's, the fragment aside.
async fn url_changed(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    before: &Baseline,
    started: tokio::time::Instant,
    observations: u64,
) -> Option<PageChange> {
    let before = before.url.as_deref()?;
    let now = read_url(page_id, browser).await?;
    let page = |url: &str| url.split('#').next().unwrap_or(url).to_owned();
    (page(before) != page(&now)).then(|| PageChange::Changed {
        evidence: vec![Evidence::Wait {
            condition: WaitCondition::Url {
                matcher: types::TextMatch::Exact(now.clone()),
            },
            elapsed_ms: started.elapsed().as_millis() as u64,
            observations,
            excluded_classes: Vec::new(),
            observed: Some(now),
        }],
        verification: "urlChanged",
    })
}

async fn control_left(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    control: &TargetSpec,
) -> Option<Vec<Evidence>> {
    let left = WaitForCommand {
        condition: WaitCondition::Element {
            target: Box::new(control.clone()),
            state: types::ElementState::Hidden,
        },
        timeout_ms: CONTROL_POLL_MS,
    };
    browser.wait_for(page_id, &left).await.ok()
}

async fn form_fields(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
) -> Option<Vec<submit_settlement::Field>> {
    let snapshot = browser.form_snapshot(page_id).await.ok()??;
    Some(submit_settlement::fields(&snapshot))
}

/// One snapshot for this poll. A failed read is "did not look", not an
/// unchanged form and not a valid form.
async fn read_settlement(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    before: &Baseline,
) -> submit_settlement::SubmitSettlementCheck {
    match submit_settlement::observe(page_id, browser, before.fields.as_deref()).await {
        Ok(settlement) => settlement,
        Err(_) => submit_settlement::SubmitSettlementCheck::unobserved(),
    }
}

fn same_fields(settlement: &submit_settlement::SubmitSettlementCheck) -> bool {
    matches!(
        settlement.field_change,
        submit_settlement::Observation::Looked(submit_settlement::FieldChange::Unchanged)
    )
}

/// Evidence that the form shows other fields than before the submit: a wait
/// on the first new field, or the settled submit when fields only left.
/// Unread and unchanged snapshots are not a change.
async fn changed_fields(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    settlement: &submit_settlement::SubmitSettlementCheck,
) -> Option<Vec<Evidence>> {
    let submit_settlement::Observation::Looked(submit_settlement::FieldChange::Changed {
        new_target,
    }) = &settlement.field_change
    else {
        return None;
    };
    if let Some(target) = new_target {
        let shown = WaitForCommand {
            condition: WaitCondition::Element {
                target: Box::new(enter_code::target_spec(target)),
                state: types::ElementState::Visible,
            },
            timeout_ms: CONTROL_POLL_MS,
        };
        if let Ok(evidence) = browser.wait_for(page_id, &shown).await {
            return Some(evidence);
        }
    }
    Some(vec![Evidence::SubmitSettlement {
        outcome: types::SubmitSettlementOutcome::Settled,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_looked_unchanged_form_counts_as_the_same_fields() {
        let unread = submit_settlement::SubmitSettlementCheck {
            field_change: submit_settlement::Observation::DidNotLook,
            issues: submit_settlement::Observation::DidNotLook,
        };
        assert!(!same_fields(&unread));
        let unchanged = submit_settlement::SubmitSettlementCheck {
            field_change: submit_settlement::Observation::Looked(
                submit_settlement::FieldChange::Unchanged,
            ),
            issues: submit_settlement::Observation::Looked(Vec::new()),
        };
        assert!(same_fields(&unchanged));
        let changed = submit_settlement::SubmitSettlementCheck {
            field_change: submit_settlement::Observation::Looked(
                submit_settlement::FieldChange::Changed { new_target: None },
            ),
            issues: submit_settlement::Observation::Looked(Vec::new()),
        };
        assert!(!same_fields(&changed));
    }
}

async fn network_quiet(page_id: &PageId, browser: &dyn IntentBrowser, timeout_ms: u64) -> bool {
    let quiet = WaitForCommand {
        condition: WaitCondition::NetworkQuiet {
            idle_ms: NETWORK_QUIET_MS,
            max_in_flight: 0,
            ignore_url_substrings: Vec::new(),
            ignore_resource_types: Vec::new(),
            ignore_long_lived: true,
        },
        timeout_ms,
    };
    browser.wait_for(page_id, &quiet).await.is_ok()
}
