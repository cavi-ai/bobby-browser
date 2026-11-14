//! Control-action fill: one route, one evidence path, one postcondition check.
//!
//! Chromium and Firefox implement [`ControlActionAdapter`] and only perform
//! the page primitive. Callers enter through [`run`].

use std::borrow::Cow;

use async_trait::async_trait;
use types::{
    CommandError, ControlAction, ControlActionCommand, ControlActionEvidence, ErrorCode,
    ErrorLayer, Evidence, FormControl, FormControlKind, FormControlState, FormControlTarget,
    FormSnapshot, PageId, RevealedControl,
};

use crate::{
    control_action_evidence, password_manager_filled, target_specs_equivalent,
    validate_control_action, PASSWORD_MANAGER_FILL_POLL, PASSWORD_MANAGER_FILL_WAIT,
};

/// Page primitives a browser adapter performs. The shared sequence lives in
/// [`run`]; an adapter does not snapshot, validate, or build evidence.
#[async_trait]
pub trait ControlActionAdapter: Send + Sync {
    type Prepared: Send;

    fn invalid_action(&self, message: String) -> CommandError {
        CommandError {
            code: ErrorCode::InvalidRequest,
            message,
            layer: ErrorLayer::Driver,
            retryable: false,
        }
    }

    fn missing_control(&self) -> CommandError {
        CommandError {
            code: ErrorCode::TargetNotFound,
            message: "form control target was not found".into(),
            layer: ErrorLayer::Driver,
            retryable: false,
        }
    }

    fn detached(&self) -> CommandError {
        CommandError {
            code: ErrorCode::TargetDetached,
            message: "form control was replaced or detached after dispatch".into(),
            layer: ErrorLayer::Driver,
            retryable: true,
        }
    }

    async fn read_snapshot(
        &self,
        page_id: &PageId,
        after: bool,
    ) -> Result<FormSnapshot, CommandError>;

    async fn resolve(
        &self,
        page_id: &PageId,
        target: &FormControlTarget,
        require_visible: bool,
    ) -> Result<Self::Prepared, CommandError>;

    async fn describe(
        &self,
        prepared: &Self::Prepared,
        target: &FormControlTarget,
    ) -> Result<Option<FormControl>, CommandError>;

    /// Re-read the control after dispatch when the first read misses it.
    async fn recover(
        &self,
        page_id: &PageId,
        target: &FormControlTarget,
        require_visible: bool,
        activated: bool,
    ) -> Result<Option<FormControl>, CommandError>;

    async fn before_primitive(&self, _prepared: &Self::Prepared) -> Result<(), CommandError> {
        Ok(())
    }

    async fn type_text(
        &self,
        page_id: &PageId,
        prepared: &Self::Prepared,
        target: &FormControlTarget,
        value: &str,
        clear_first: bool,
    ) -> Result<(), CommandError>;

    async fn set_checked(
        &self,
        page_id: &PageId,
        prepared: &Self::Prepared,
        target: &FormControlTarget,
        checked: bool,
    ) -> Result<(), CommandError>;

    async fn select_one(
        &self,
        page_id: &PageId,
        prepared: &Self::Prepared,
        target: &FormControlTarget,
        value: &str,
        kind: FormControlKind,
    ) -> Result<String, CommandError>;

    /// `Some` when the adapter resolved the committed option values.
    async fn select_many(
        &self,
        page_id: &PageId,
        prepared: &Self::Prepared,
        target: &FormControlTarget,
        values: &[String],
        kind: FormControlKind,
    ) -> Result<Option<Vec<String>>, CommandError>;

    async fn set_files(
        &self,
        page_id: &PageId,
        prepared: &Self::Prepared,
        target: &FormControlTarget,
        before: &FormControl,
        paths: &[String],
    ) -> Result<(), CommandError>;

    async fn clear(
        &self,
        page_id: &PageId,
        prepared: &Self::Prepared,
        target: &FormControlTarget,
        kind: FormControlKind,
    ) -> Result<(), CommandError>;

    async fn activate(
        &self,
        page_id: &PageId,
        prepared: &Self::Prepared,
        target: &FormControlTarget,
    ) -> Result<(), CommandError>;

    async fn fill_credential(
        &self,
        page_id: &PageId,
        prepared: &Self::Prepared,
        target: &FormControlTarget,
        name: &str,
    ) -> Result<(), CommandError>;

    fn decorate(&self, evidence: Vec<Evidence>) -> Vec<Evidence> {
        evidence
    }
}

/// Validate, read, dispatch one primitive, and emit control-action evidence.
pub async fn run<A: ControlActionAdapter + ?Sized>(
    adapter: &A,
    page_id: &PageId,
    command: &ControlActionCommand,
) -> Result<Vec<Evidence>, CommandError> {
    if let Err(message) = command.action.validate() {
        return Err(adapter.invalid_action(message));
    }
    let require_visible = command.target.attributes.get("type").map(String::as_str) != Some("file");
    let prepared = adapter
        .resolve(page_id, &command.target, require_visible)
        .await?;
    let before = adapter.read_snapshot(page_id, false).await?;
    let before_control = adapter
        .describe(&prepared, &command.target)
        .await?
        .ok_or_else(|| adapter.missing_control())?;
    validate_control_action(&before_control, &command.action)?;
    adapter.before_primitive(&prepared).await?;

    let mut committed: Option<Vec<String>> = None;
    match &command.action {
        ControlAction::SetText { value, clear_first } => {
            adapter
                .type_text(page_id, &prepared, &command.target, value, *clear_first)
                .await?;
        }
        ControlAction::SetChecked { checked } => {
            adapter
                .set_checked(page_id, &prepared, &command.target, *checked)
                .await?;
        }
        ControlAction::SelectOne { value } => {
            committed = Some(vec![
                adapter
                    .select_one(
                        page_id,
                        &prepared,
                        &command.target,
                        value,
                        before_control.control_kind,
                    )
                    .await?,
            ]);
        }
        ControlAction::SelectMany { values } => {
            committed = adapter
                .select_many(
                    page_id,
                    &prepared,
                    &command.target,
                    values,
                    before_control.control_kind,
                )
                .await?;
        }
        ControlAction::SetFiles { paths } => {
            adapter
                .set_files(page_id, &prepared, &command.target, &before_control, paths)
                .await?;
        }
        ControlAction::Clear => {
            adapter
                .clear(
                    page_id,
                    &prepared,
                    &command.target,
                    before_control.control_kind,
                )
                .await?;
        }
        ControlAction::Activate => {
            adapter
                .activate(page_id, &prepared, &command.target)
                .await?;
        }
        ControlAction::Credential { name } => {
            adapter
                .fill_credential(page_id, &prepared, &command.target, name)
                .await?;
        }
        ControlAction::PasswordManagerFill => {
            // Only watches: the field is never clicked, focused, or typed
            // into, which would raise the browser's own credential dialogs.
            let deadline = tokio::time::Instant::now() + PASSWORD_MANAGER_FILL_WAIT;
            while tokio::time::Instant::now() < deadline {
                match adapter.describe(&prepared, &command.target).await {
                    Ok(Some(control)) if !password_manager_filled(&control.state) => {
                        tokio::time::sleep(PASSWORD_MANAGER_FILL_POLL).await;
                    }
                    _ => break,
                }
            }
        }
    }

    let activated = matches!(command.action, ControlAction::Activate);
    let after = match adapter.read_snapshot(page_id, true).await {
        Ok(after) => Some(after),
        Err(_) if activated => None,
        Err(error) => return Err(error),
    };
    let after_control = match adapter.describe(&prepared, &command.target).await {
        Ok(Some(control)) => Some(control),
        _ => {
            adapter
                .recover(page_id, &command.target, require_visible, activated)
                .await?
        }
    };
    let node_replaced = after_control.is_none();
    let after_control = match after_control {
        Some(control) => control,
        None if activated => before_control.clone(),
        None => return Err(adapter.detached()),
    };
    let matched = evidence_action(&command.action, &after_control);
    let mut action = control_action_evidence(
        &after_control,
        matched.as_ref(),
        node_replaced,
        committed.as_deref(),
    )?;
    action.revealed_controls = revealed_controls(&before, after.as_ref());
    // Value landed is `control_action_evidence`. Validity, committed
    // selection, and redaction stay on the evidence for the caller to read.
    verify_evidence(&command.action, &action)?;
    Ok(adapter.decorate(vec![Evidence::ControlAction { action }]))
}

/// User-visible postconditions on the one evidence record: the value landed
/// (already required by [`control_action_evidence`]), validity is reported,
/// a committed selection is present for selects, and secrets stay redacted.
///
/// A control the browser marked invalid still returns evidence; this rejects
/// only a missing selection or a secret left in clear text.
pub fn verify_evidence(
    action: &ControlAction,
    evidence: &ControlActionEvidence,
) -> Result<(), CommandError> {
    match action {
        ControlAction::SelectOne { .. } => match &evidence.state {
            FormControlState::Selection { values } if values.len() == 1 => Ok(()),
            _ => Err(postcondition("committed selection was not observed")),
        },
        ControlAction::SelectMany { values } => match &evidence.state {
            FormControlState::Selection { values: observed } if observed.len() == values.len() => {
                Ok(())
            }
            _ => Err(postcondition("committed selection was not observed")),
        },
        ControlAction::Credential { .. } => match evidence.state {
            FormControlState::Redacted { .. } => Ok(()),
            _ => Err(postcondition(
                "the credential did not stay redacted on the control",
            )),
        },
        _ => Ok(()),
    }
}

fn postcondition(message: &str) -> CommandError {
    CommandError {
        code: ErrorCode::VerificationFailed,
        message: message.to_owned(),
        layer: ErrorLayer::Browser,
        retryable: false,
    }
}

/// A read-back the strict equality check would reject, but that still shows
/// the typed value: an input mask's phone formatting, or a field whose text
/// contains what was typed. The evidence state stays the read-back.
fn evidence_action<'a>(action: &'a ControlAction, control: &FormControl) -> Cow<'a, ControlAction> {
    match (action, &control.state) {
        (
            ControlAction::SetText { value, clear_first },
            FormControlState::Text { value: actual },
        ) if actual != value
            && (actual.contains(value.as_str()) || same_phone_number(value, actual)) =>
        {
            Cow::Owned(ControlAction::SetText {
                value: actual.clone(),
                clear_first: *clear_first,
            })
        }
        _ => Cow::Borrowed(action),
    }
}

/// Whether `read` is the phone number `typed` reformatted by an input mask.
fn same_phone_number(typed: &str, read: &str) -> bool {
    fn digits(value: &str) -> Option<String> {
        value
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, ' ' | '-' | '.' | '(' | ')' | '+'))
            .then(|| value.chars().filter(char::is_ascii_digit).collect())
    }
    match (digits(typed), digits(read)) {
        (Some(typed), Some(read)) => typed.len() >= 7 && typed == read,
        _ => false,
    }
}

fn revealed_controls(before: &FormSnapshot, after: Option<&FormSnapshot>) -> Vec<RevealedControl> {
    let before_targets: Vec<&FormControlTarget> = before
        .forms
        .iter()
        .flat_map(|form| form.controls.iter())
        .chain(before.unowned_controls.iter())
        .filter_map(|control| control.target.as_ref())
        .collect();
    after
        .iter()
        .flat_map(|after| after.forms.iter().flat_map(|form| form.controls.iter()))
        .chain(after.iter().flat_map(|after| after.unowned_controls.iter()))
        .filter(|control| {
            control.target.as_ref().is_some_and(|target| {
                !before_targets
                    .iter()
                    .any(|before| target_specs_equivalent(before, target))
            })
        })
        .take(8)
        .map(|control| RevealedControl {
            control_kind: control.control_kind,
            accessible_name: control.accessible_name.clone(),
            target: control.target.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;
    use types::{
        FormControlConstraints, FormControlOperation, FormControlValidity,
        FORM_SNAPSHOT_SCHEMA_VERSION,
    };

    struct FakeAdapter {
        calls: Mutex<Vec<&'static str>>,
        dispatched: AtomicBool,
        kind: FormControlKind,
        operations: Vec<FormControlOperation>,
        before: FormControlState,
        after: FormControlState,
        committed: Option<Vec<String>>,
        valid: bool,
        reveal: bool,
        snapshots: AtomicUsize,
    }

    impl FakeAdapter {
        fn text(before: &str, after: &str) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                dispatched: AtomicBool::new(false),
                kind: FormControlKind::Text,
                operations: vec![FormControlOperation::SetText, FormControlOperation::Clear],
                before: FormControlState::Text {
                    value: before.into(),
                },
                after: FormControlState::Text {
                    value: after.into(),
                },
                committed: None,
                valid: true,
                reveal: false,
                snapshots: AtomicUsize::new(0),
            }
        }

        fn control(&self, state: FormControlState, name: &str) -> FormControl {
            FormControl {
                id: name.into(),
                form_id: None,
                group_id: None,
                target: Some(target(name)),
                control_kind: self.kind,
                accessible_name: Some(name.into()),
                label: None,
                description: None,
                placeholder: None,
                autocomplete: None,
                state,
                constraints: FormControlConstraints::default(),
                validity: FormControlValidity {
                    will_validate: true,
                    valid: self.valid,
                    flags: Vec::new(),
                    message: None,
                    described_by: Vec::new(),
                },
                options: Vec::new(),
                supported_operations: self.operations.clone(),
            }
        }
    }

    fn target(name: &str) -> FormControlTarget {
        FormControlTarget {
            attributes: Default::default(),
            role: "textbox".into(),
            accessible_name: name.into(),
            ordinal: None,
            frame_path: Vec::new(),
            shadow_path: Vec::new(),
        }
    }

    fn snapshot(controls: Vec<FormControl>) -> FormSnapshot {
        FormSnapshot {
            schema_version: FORM_SNAPSHOT_SCHEMA_VERSION,
            page_id: PageId::new(),
            forms: Vec::new(),
            unowned_controls: controls,
            truncated: false,
        }
    }

    #[async_trait]
    impl ControlActionAdapter for FakeAdapter {
        type Prepared = ();

        async fn read_snapshot(
            &self,
            _page_id: &PageId,
            _after: bool,
        ) -> Result<FormSnapshot, CommandError> {
            let n = self.snapshots.fetch_add(1, Ordering::SeqCst);
            let mut controls = vec![self.control(
                if self.dispatched.load(Ordering::SeqCst) {
                    self.after.clone()
                } else {
                    self.before.clone()
                },
                "Email",
            )];
            if self.reveal && n > 0 {
                let mut extra = self.control(FormControlState::Empty, "Billing cycle");
                extra.control_kind = FormControlKind::Combobox;
                extra.target = Some(target("Billing cycle"));
                controls.push(extra);
            }
            Ok(snapshot(controls))
        }

        async fn resolve(
            &self,
            _page_id: &PageId,
            _target: &FormControlTarget,
            _require_visible: bool,
        ) -> Result<(), CommandError> {
            self.calls.lock().expect("calls").push("resolve");
            Ok(())
        }

        async fn describe(
            &self,
            _prepared: &(),
            target: &FormControlTarget,
        ) -> Result<Option<FormControl>, CommandError> {
            let name = target.accessible_name.as_str();
            if name == "Billing cycle" {
                return Ok(None);
            }
            let state = if self.dispatched.load(Ordering::SeqCst) {
                self.after.clone()
            } else {
                self.before.clone()
            };
            Ok(Some(self.control(state, "Email")))
        }

        async fn recover(
            &self,
            _page_id: &PageId,
            _target: &FormControlTarget,
            _require_visible: bool,
            _activated: bool,
        ) -> Result<Option<FormControl>, CommandError> {
            Ok(None)
        }

        async fn type_text(
            &self,
            _: &PageId,
            _: &(),
            _: &FormControlTarget,
            _: &str,
            _: bool,
        ) -> Result<(), CommandError> {
            self.calls.lock().expect("calls").push("type");
            self.dispatched.store(true, Ordering::SeqCst);
            Ok(())
        }

        async fn set_checked(
            &self,
            _: &PageId,
            _: &(),
            _: &FormControlTarget,
            _: bool,
        ) -> Result<(), CommandError> {
            self.calls.lock().expect("calls").push("set_checked");
            self.dispatched.store(true, Ordering::SeqCst);
            Ok(())
        }

        async fn select_one(
            &self,
            _: &PageId,
            _: &(),
            _: &FormControlTarget,
            _: &str,
            _: FormControlKind,
        ) -> Result<String, CommandError> {
            self.calls.lock().expect("calls").push("select");
            self.dispatched.store(true, Ordering::SeqCst);
            Ok(self
                .committed
                .as_ref()
                .and_then(|values| values.first().cloned())
                .unwrap_or_default())
        }

        async fn select_many(
            &self,
            _: &PageId,
            _: &(),
            _: &FormControlTarget,
            _: &[String],
            _: FormControlKind,
        ) -> Result<Option<Vec<String>>, CommandError> {
            self.calls.lock().expect("calls").push("select");
            self.dispatched.store(true, Ordering::SeqCst);
            Ok(self.committed.clone())
        }

        async fn set_files(
            &self,
            _: &PageId,
            _: &(),
            _: &FormControlTarget,
            _: &FormControl,
            _: &[String],
        ) -> Result<(), CommandError> {
            self.calls.lock().expect("calls").push("upload");
            self.dispatched.store(true, Ordering::SeqCst);
            Ok(())
        }

        async fn clear(
            &self,
            _: &PageId,
            _: &(),
            _: &FormControlTarget,
            _: FormControlKind,
        ) -> Result<(), CommandError> {
            self.calls.lock().expect("calls").push("clear");
            self.dispatched.store(true, Ordering::SeqCst);
            Ok(())
        }

        async fn activate(
            &self,
            _: &PageId,
            _: &(),
            _: &FormControlTarget,
        ) -> Result<(), CommandError> {
            self.calls.lock().expect("calls").push("activate");
            self.dispatched.store(true, Ordering::SeqCst);
            Ok(())
        }

        async fn fill_credential(
            &self,
            _: &PageId,
            _: &(),
            _: &FormControlTarget,
            _: &str,
        ) -> Result<(), CommandError> {
            self.calls.lock().expect("calls").push("credential");
            self.dispatched.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    fn command(action: ControlAction) -> ControlActionCommand {
        ControlActionCommand {
            target: target("Email"),
            action,
        }
    }

    fn evidence_of(evidence: Vec<Evidence>) -> ControlActionEvidence {
        evidence
            .into_iter()
            .find_map(|item| match item {
                Evidence::ControlAction { action } => Some(action),
                _ => None,
            })
            .expect("control-action evidence")
    }

    #[tokio::test]
    async fn fill_routes_text_and_reports_one_evidence_path() {
        let adapter = FakeAdapter::text("", "Ada");
        let evidence = run(
            &adapter,
            &PageId::new(),
            &command(ControlAction::SetText {
                value: "Ada".into(),
                clear_first: true,
            }),
        )
        .await
        .expect("fill");
        assert_eq!(
            adapter.calls.lock().expect("calls").as_slice(),
            ["resolve", "type"]
        );
        let action = evidence_of(evidence);
        assert!(action.validity.valid);
        assert_eq!(
            action.state,
            FormControlState::Text {
                value: "Ada".into()
            }
        );
        verify_evidence(
            &ControlAction::SetText {
                value: "Ada".into(),
                clear_first: true,
            },
            &action,
        )
        .unwrap();
    }

    #[tokio::test]
    async fn a_phone_mask_still_counts_as_the_typed_number() {
        let adapter = FakeAdapter::text("", "(555) 010-0199");
        let evidence = run(
            &adapter,
            &PageId::new(),
            &command(ControlAction::SetText {
                value: "555-010-0199".into(),
                clear_first: true,
            }),
        )
        .await
        .expect("phone fill");
        let action = evidence_of(evidence);
        assert_eq!(
            action.state,
            FormControlState::Text {
                value: "(555) 010-0199".into()
            }
        );
    }

    #[tokio::test]
    async fn credential_evidence_redacts_the_filled_secret() {
        let secret = "canary-secret-value";
        let mut adapter = FakeAdapter::text("", secret);
        adapter.kind = FormControlKind::Password;
        adapter.operations = vec![FormControlOperation::SetText];
        let evidence = run(
            &adapter,
            &PageId::new(),
            &command(ControlAction::Credential {
                name: "login".into(),
            }),
        )
        .await
        .expect("credential");
        assert_eq!(
            adapter.calls.lock().expect("calls").as_slice(),
            ["resolve", "credential"]
        );
        let action = evidence_of(evidence);
        assert_eq!(action.state, FormControlState::Redacted { present: true });
        let rendered = format!("{action:?}");
        assert!(!rendered.contains(secret), "{rendered}");
        verify_evidence(
            &ControlAction::Credential {
                name: "login".into(),
            },
            &action,
        )
        .unwrap();
    }

    #[tokio::test]
    async fn select_keeps_the_committed_value_when_the_request_used_a_label() {
        let mut adapter = FakeAdapter::text("", "");
        adapter.kind = FormControlKind::SelectOne;
        adapter.operations = vec![FormControlOperation::SelectOne];
        adapter.before = FormControlState::Selection {
            values: vec!["us".into()],
        };
        adapter.after = FormControlState::Selection {
            values: vec!["us".into()],
        };
        adapter.committed = Some(vec!["us".into()]);
        let evidence = run(
            &adapter,
            &PageId::new(),
            &command(ControlAction::SelectOne {
                value: "United States".into(),
            }),
        )
        .await
        .expect("select");
        assert_eq!(
            adapter.calls.lock().expect("calls").as_slice(),
            ["resolve", "select"]
        );
        let action = evidence_of(evidence);
        verify_evidence(
            &ControlAction::SelectOne {
                value: "United States".into(),
            },
            &action,
        )
        .unwrap();
        assert_eq!(
            action.state,
            FormControlState::Selection {
                values: vec!["us".into()]
            }
        );
        let rendered = format!("{action:?}");
        assert!(!rendered.contains("United States"), "{rendered}");
    }

    #[tokio::test]
    async fn a_rejected_postcondition_does_not_quote_the_typed_value() {
        let secret = "not-landed-secret";
        let adapter = FakeAdapter::text("", "something-else");
        let error = run(
            &adapter,
            &PageId::new(),
            &command(ControlAction::SetText {
                value: secret.into(),
                clear_first: true,
            }),
        )
        .await
        .expect_err("mismatch");
        assert_eq!(error.code, ErrorCode::VerificationFailed);
        assert!(!error.message.contains(secret), "{}", error.message);
    }

    #[tokio::test]
    async fn revealed_controls_come_back_on_the_same_evidence() {
        let mut adapter = FakeAdapter::text("", "growth");
        adapter.reveal = true;
        let action = evidence_of(
            run(
                &adapter,
                &PageId::new(),
                &command(ControlAction::SetText {
                    value: "growth".into(),
                    clear_first: true,
                }),
            )
            .await
            .expect("fill"),
        );
        assert_eq!(action.revealed_controls.len(), 1);
        assert_eq!(
            action.revealed_controls[0].accessible_name.as_deref(),
            Some("Billing cycle")
        );
    }
}
