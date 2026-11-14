//! One form-snapshot read, plus the aria-invalid census, for a submit.
//!
//! [`Observation::DidNotLook`] means that read did not show the form (and the
//! census named no invalid control). It is not an unchanged form and it is
//! not a form with no validation issues. [`Observation::Looked`] is what the
//! read showed.

use dom_engine::Candidate;

use super::*;

const MAX_ISSUES: usize = 512;

pub type Field = (types::FormControlKind, String);

/// A settlement fact that was either not observed or observed.
#[derive(Debug, PartialEq, Eq)]
pub enum Observation<T> {
    DidNotLook,
    Looked(T),
}

/// How the form's fields compare with the pre-submit baseline.
#[derive(Debug, PartialEq, Eq)]
pub enum FieldChange {
    /// The snapshot was read and lists the same fields as the baseline.
    Unchanged,
    /// The snapshot was read and lists other fields. `new_target` is the
    /// first new control that names a target.
    Changed {
        new_target: Option<types::FormControlTarget>,
    },
}

/// Field change and validation issues from one snapshot and one census.
#[derive(Debug, PartialEq, Eq)]
pub struct SubmitSettlementCheck {
    pub field_change: Observation<FieldChange>,
    pub issues: Observation<Vec<types::FormValidationIssue>>,
}

impl SubmitSettlementCheck {
    pub(super) fn unobserved() -> Self {
        Self {
            field_change: Observation::DidNotLook,
            issues: Observation::DidNotLook,
        }
    }
}

/// Reads the form snapshot once and the aria-invalid census once.
pub async fn observe(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    before: Option<&[Field]>,
) -> Result<SubmitSettlementCheck, CommandError> {
    let snapshot = browser.form_snapshot(page_id).await?;
    let census = browser
        .collect_candidates(page_id, &aria_invalid_probe())
        .await?;
    Ok(assess(before, snapshot.as_ref(), &census))
}

fn aria_invalid_probe() -> TargetSpec {
    TargetSpec {
        css: Some("[aria-invalid='true']".into()),
        ..TargetSpec::default()
    }
}

/// Joins one snapshot with the aria-invalid census by accessible name.
///
/// `before` is the pre-submit field list. Without it, or without a snapshot,
/// the field change is [`Observation::DidNotLook`], not unchanged.
pub fn assess(
    before: Option<&[Field]>,
    snapshot: Option<&types::FormSnapshot>,
    aria_invalid: &[Candidate],
) -> SubmitSettlementCheck {
    SubmitSettlementCheck {
        field_change: compare_fields(before, snapshot),
        issues: validation_issues(snapshot, aria_invalid),
    }
}

/// Field identity kept for a later settlement comparison, sorted by name.
pub fn fields(snapshot: &types::FormSnapshot) -> Vec<Field> {
    let mut fields: Vec<Field> = controls(snapshot).map(field_key).collect();
    fields.sort_by(|left, right| left.1.cmp(&right.1));
    fields
}

fn compare_fields(
    before: Option<&[Field]>,
    snapshot: Option<&types::FormSnapshot>,
) -> Observation<FieldChange> {
    let (Some(before), Some(snapshot)) = (before, snapshot) else {
        return Observation::DidNotLook;
    };
    let mut baseline = before.to_vec();
    baseline.sort_by(|left, right| left.1.cmp(&right.1));
    let now = fields(snapshot);
    if now == baseline {
        return Observation::Looked(FieldChange::Unchanged);
    }
    let new_target = controls(snapshot).find_map(|control| {
        if baseline.contains(&field_key(control)) {
            None
        } else {
            control.target.clone()
        }
    });
    Observation::Looked(FieldChange::Changed { new_target })
}

fn validation_issues(
    snapshot: Option<&types::FormSnapshot>,
    aria_invalid: &[Candidate],
) -> Observation<Vec<types::FormValidationIssue>> {
    let visible_aria_invalid = aria_invalid
        .iter()
        .filter(|candidate| candidate.state.attached && candidate.state.visible)
        .filter(|candidate| {
            candidate
                .attributes
                .get("aria-invalid")
                .is_some_and(|value| value.eq_ignore_ascii_case("true"))
        })
        .collect::<Vec<_>>();
    let mut issues = Vec::new();
    if let Some(snapshot) = snapshot {
        for control in controls(snapshot) {
            let aria_rejected = control.accessible_name.as_deref().is_some_and(|name| {
                visible_aria_invalid
                    .iter()
                    .filter_map(|candidate| candidate.name.as_deref())
                    .any(|candidate_name| candidate_name.eq_ignore_ascii_case(name))
            });
            if !control.validity.valid || aria_rejected {
                let mut validity = control.validity.clone();
                if aria_rejected && validity.valid {
                    validity.valid = false;
                    validity.flags.push(types::FormValidityFlag::CustomError);
                }
                issues.push(types::FormValidationIssue {
                    control_id: control.id.clone(),
                    control_kind: control.control_kind,
                    accessible_name: control.accessible_name.clone(),
                    target: control.target.clone(),
                    validity,
                });
                if issues.len() == MAX_ISSUES {
                    return Observation::Looked(issues);
                }
            }
        }
    }

    for candidate in visible_aria_invalid {
        let already_described = issues.iter().any(|issue| {
            issue.control_id == candidate.id
                || issue.accessible_name.as_deref().is_some_and(|issue_name| {
                    candidate.name.as_deref().is_some_and(|candidate_name| {
                        issue_name.eq_ignore_ascii_case(candidate_name)
                    })
                })
        });
        if already_described {
            continue;
        }
        issues.push(types::FormValidationIssue {
            control_id: candidate.id.clone(),
            control_kind: types::FormControlKind::Other,
            accessible_name: candidate.name.clone(),
            target: None,
            validity: types::FormControlValidity {
                will_validate: false,
                valid: false,
                flags: vec![types::FormValidityFlag::CustomError],
                message: None,
                described_by: Vec::new(),
            },
        });
        if issues.len() == MAX_ISSUES {
            break;
        }
    }

    if snapshot.is_none() && issues.is_empty() {
        Observation::DidNotLook
    } else {
        Observation::Looked(issues)
    }
}

fn controls(snapshot: &types::FormSnapshot) -> impl Iterator<Item = &types::FormControl> {
    snapshot
        .forms
        .iter()
        .flat_map(|form| form.controls.iter())
        .chain(snapshot.unowned_controls.iter())
}

fn field_key(control: &types::FormControl) -> Field {
    (
        control.control_kind,
        control.accessible_name.clone().unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn control(id: &str, name: &str, valid: bool) -> types::FormControl {
        types::FormControl {
            id: id.into(),
            form_id: Some("form-1".into()),
            group_id: None,
            target: None,
            control_kind: types::FormControlKind::Text,
            accessible_name: Some(name.into()),
            label: None,
            description: None,
            placeholder: None,
            autocomplete: None,
            state: types::FormControlState::Empty,
            constraints: types::FormControlConstraints {
                required: false,
                read_only: false,
                disabled: false,
                pattern: None,
                min_length: None,
                max_length: None,
                min: None,
                max: None,
                step: None,
                multiple: false,
                accept: Vec::new(),
            },
            validity: types::FormControlValidity {
                will_validate: true,
                valid,
                flags: if valid {
                    Vec::new()
                } else {
                    vec![types::FormValidityFlag::ValueMissing]
                },
                message: (!valid).then(|| format!("{name} is required")),
                described_by: Vec::new(),
            },
            options: Vec::new(),
            supported_operations: vec![types::FormControlOperation::SetText],
        }
    }

    fn snapshot(controls: Vec<types::FormControl>) -> types::FormSnapshot {
        let valid = controls.iter().all(|control| control.validity.valid);
        types::FormSnapshot {
            schema_version: types::FORM_SNAPSHOT_SCHEMA_VERSION,
            page_id: PageId::new(),
            forms: vec![types::FormDescriptor {
                id: "form-1".into(),
                target: None,
                accessible_name: Some("Customer onboarding".into()),
                description: None,
                groups: Vec::new(),
                controls,
                submit_control_ids: Vec::new(),
                reset_control_ids: Vec::new(),
                validity: types::FormValidity {
                    valid,
                    invalid_control_ids: if valid {
                        Vec::new()
                    } else {
                        vec!["postal-code".into()]
                    },
                },
            }],
            unowned_controls: Vec::new(),
            truncated: false,
        }
    }

    fn candidate(id: &str, name: Option<&str>, aria_invalid: bool, visible: bool) -> Candidate {
        let mut attributes = BTreeMap::new();
        if aria_invalid {
            attributes.insert("aria-invalid".into(), "true".into());
        }
        Candidate {
            id: id.into(),
            css: Some(format!("#{id}")),
            tag: None,
            test_id: None,
            role: Some("textbox".into()),
            name: name.map(str::to_owned),
            label: None,
            text: String::new(),
            attributes,
            state: dom_engine::CandidateState {
                attached: true,
                visible,
                enabled: true,
            },
            frame_path: Vec::new(),
        }
    }

    #[test]
    fn an_unread_snapshot_is_not_an_unchanged_or_valid_form() {
        let check = assess(Some(&[]), None, &[]);
        assert_eq!(check.field_change, Observation::DidNotLook);
        assert_eq!(check.issues, Observation::DidNotLook);
    }

    #[test]
    fn a_missing_baseline_is_not_an_unchanged_form() {
        let seen = snapshot(vec![control("postal-code", "Postal code", true)]);
        let check = assess(None, Some(&seen), &[]);
        assert_eq!(check.field_change, Observation::DidNotLook);
        assert!(matches!(
            check.issues,
            Observation::Looked(issues) if issues.is_empty()
        ));
    }

    #[test]
    fn the_same_fields_in_any_order_are_unchanged_and_valid() {
        let postal = control("postal-code", "Postal code", true);
        let name = control("full-name", "Full name", true);
        let seen = snapshot(vec![name.clone(), postal.clone()]);
        let before = fields(&snapshot(vec![postal, name]));
        let check = assess(Some(&before), Some(&seen), &[]);
        assert_eq!(
            check.field_change,
            Observation::Looked(FieldChange::Unchanged)
        );
        assert!(matches!(
            check.issues,
            Observation::Looked(issues) if issues.is_empty()
        ));
    }

    #[test]
    fn a_new_field_is_a_change_and_names_its_target() {
        let mut added = control("code", "Code", true);
        added.target = Some(types::FormControlTarget {
            attributes: Box::default(),
            role: "textbox".into(),
            accessible_name: "Code".into(),
            ordinal: None,
            frame_path: Vec::new(),
            shadow_path: Vec::new(),
        });
        let kept = control("postal-code", "Postal code", true);
        let before = fields(&snapshot(vec![kept.clone()]));
        let check = assess(
            Some(&before),
            Some(&snapshot(vec![kept, added.clone()])),
            &[],
        );
        assert_eq!(
            check.field_change,
            Observation::Looked(FieldChange::Changed {
                new_target: added.target,
            })
        );
    }

    #[test]
    fn validity_and_aria_invalid_names_reject_only_the_matching_controls() {
        let postal = control("postal-code", "Postal code", true);
        let healthy = control("full-name", "Full name", true);
        let seen = snapshot(vec![postal.clone(), healthy]);
        let aria = candidate("postal-code", Some("postal CODE"), true, true);
        let healthy_candidate = candidate("full-name", Some("Full name"), false, true);
        let check = assess(None, Some(&seen), &[aria, healthy_candidate]);
        let Observation::Looked(issues) = check.issues else {
            panic!("a read snapshot must report its issues");
        };
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].accessible_name.as_deref(), Some("Postal code"));
        assert!(!issues[0].validity.valid);
        assert_eq!(
            issues[0].validity.flags,
            vec![types::FormValidityFlag::CustomError]
        );

        let native = assess(
            None,
            Some(&snapshot(vec![control(
                "postal-code",
                "Postal code",
                false,
            )])),
            &[],
        );
        let Observation::Looked(issues) = native.issues else {
            panic!("native validity is a looked result");
        };
        assert_eq!(issues.len(), 1);
        assert_eq!(
            issues[0].validity.message.as_deref(),
            Some("Postal code is required")
        );
    }

    #[test]
    fn a_visible_aria_invalid_control_rejects_without_a_snapshot() {
        let hidden = candidate("hidden", Some("Hidden"), true, false);
        let unnamed = candidate("unnamed-invalid", None, true, true);
        let check = assess(Some(&[]), None, &[hidden, unnamed]);
        let Observation::Looked(issues) = check.issues else {
            panic!("the census was read");
        };
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].control_id, "unnamed-invalid");
        assert_eq!(issues[0].accessible_name, None);
        assert!(!issues[0].validity.valid);
        assert_eq!(check.field_change, Observation::DidNotLook);
    }

    #[test]
    fn an_invalid_unowned_control_is_an_issue() {
        let mut seen = snapshot(Vec::new());
        seen.unowned_controls.push(control("notes", "Notes", false));
        let check = assess(None, Some(&seen), &[]);
        let Observation::Looked(issues) = check.issues else {
            panic!("unowned controls are part of the snapshot");
        };
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].control_id, "notes");
    }
}
