//! Form snapshot and control-action wire types.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize, Serializer};

use crate::PageId;

/// Schema version for [`FormSnapshot`] payloads.
pub const FORM_SNAPSHOT_SCHEMA_VERSION: u16 = 1;
pub const MAX_FORM_SNAPSHOT_FORMS: usize = 64;
pub const MAX_FORM_SNAPSHOT_CONTROLS: usize = 512;
pub const MAX_FORM_GROUPS: usize = 128;
pub const MAX_FORM_OPTIONS: usize = 512;
pub const MAX_FORM_REFERENCES: usize = 512;
pub const MAX_FORM_ACCEPT_TYPES: usize = 128;
pub const MAX_FORM_TARGET_PATH: usize = 8;
pub const MAX_FORM_TARGET_ORDINAL: usize = 2_047;
pub const MAX_FORM_ID_BYTES: usize = 128;
pub const MAX_FORM_TEXT_BYTES: usize = 2_048;
pub const MAX_FORM_VALUE_BYTES: usize = 4_096;
pub const MAX_FORM_VALIDATION_MESSAGE_BYTES: usize = 1_024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SemanticTargetSegment {
    pub role: String,
    pub accessible_name: String,
    pub ordinal: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FormControlTarget {
    pub role: String,
    pub accessible_name: String,
    pub ordinal: Option<usize>,
    pub frame_path: Vec<SemanticTargetSegment>,
    pub shadow_path: Vec<SemanticTargetSegment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum FormControlKind {
    Text,
    Email,
    Password,
    Search,
    Number,
    Checkbox,
    Radio,
    Switch,
    SelectOne,
    SelectMultiple,
    Date,
    Time,
    DateTimeLocal,
    Range,
    File,
    ContentEditable,
    Combobox,
    Listbox,
    Submit,
    Reset,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum FormControlState {
    Empty,
    Text { value: String },
    Redacted { present: bool },
    Checked { checked: bool },
    Selection { values: Vec<String> },
    Files { count: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum FormControlOperation {
    SetText,
    SetChecked,
    SelectOne,
    SelectMany,
    SetFiles,
    Clear,
    Activate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FormControlConstraints {
    pub required: bool,
    pub read_only: bool,
    pub disabled: bool,
    pub pattern: Option<String>,
    pub min_length: Option<u32>,
    pub max_length: Option<u32>,
    pub min: Option<String>,
    pub max: Option<String>,
    pub step: Option<String>,
    pub multiple: bool,
    pub accept: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum FormValidityFlag {
    ValueMissing,
    TypeMismatch,
    PatternMismatch,
    TooLong,
    TooShort,
    RangeUnderflow,
    RangeOverflow,
    StepMismatch,
    BadInput,
    CustomError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FormControlValidity {
    pub will_validate: bool,
    pub valid: bool,
    pub flags: Vec<FormValidityFlag>,
    pub message: Option<String>,
    pub described_by: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FormOption {
    pub value: String,
    pub label: String,
    pub disabled: bool,
    pub selected: bool,
    pub group_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FormControl {
    pub id: String,
    pub form_id: Option<String>,
    pub group_id: Option<String>,
    pub target: Option<FormControlTarget>,
    pub control_kind: FormControlKind,
    pub accessible_name: Option<String>,
    pub label: Option<String>,
    pub description: Option<String>,
    pub placeholder: Option<String>,
    pub autocomplete: Option<String>,
    pub state: FormControlState,
    pub constraints: FormControlConstraints,
    pub validity: FormControlValidity,
    pub options: Vec<FormOption>,
    pub supported_operations: Vec<FormControlOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FormGroup {
    pub id: String,
    pub label: Option<String>,
    pub description: Option<String>,
    pub control_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FormValidity {
    pub valid: bool,
    pub invalid_control_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FormDescriptor {
    pub id: String,
    pub target: Option<FormControlTarget>,
    pub accessible_name: Option<String>,
    pub description: Option<String>,
    pub groups: Vec<FormGroup>,
    pub controls: Vec<FormControl>,
    pub submit_control_ids: Vec<String>,
    pub reset_control_ids: Vec<String>,
    pub validity: FormValidity,
}

/// Semantic form observation returned by form-snapshot endpoints and evidence.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(
    rename_all = "camelCase",
    deny_unknown_fields,
    try_from = "FormSnapshotWire"
)]
pub struct FormSnapshot {
    pub schema_version: u16,
    pub page_id: PageId,
    pub forms: Vec<FormDescriptor>,
    pub unowned_controls: Vec<FormControl>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FormSnapshotWire {
    schema_version: u16,
    page_id: PageId,
    #[cfg_attr(feature = "schema", schemars(length(max = 64)))]
    forms: Vec<FormDescriptor>,
    #[cfg_attr(feature = "schema", schemars(length(max = 512)))]
    unowned_controls: Vec<FormControl>,
    truncated: bool,
}

impl TryFrom<FormSnapshotWire> for FormSnapshot {
    type Error = String;

    fn try_from(wire: FormSnapshotWire) -> Result<Self, Self::Error> {
        let snapshot = Self {
            schema_version: wire.schema_version,
            page_id: wire.page_id,
            forms: wire.forms,
            unowned_controls: wire.unowned_controls,
            truncated: wire.truncated,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }
}

impl Serialize for FormSnapshot {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        FormSnapshotWire {
            schema_version: self.schema_version,
            page_id: self.page_id.clone(),
            forms: self.forms.clone(),
            unowned_controls: self.unowned_controls.clone(),
            truncated: self.truncated,
        }
        .serialize(serializer)
    }
}

impl FormSnapshot {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != FORM_SNAPSHOT_SCHEMA_VERSION {
            return Err("unsupported form snapshot schema version".into());
        }
        if self.forms.len() > MAX_FORM_SNAPSHOT_FORMS {
            return Err("form snapshot exceeds its form bound".into());
        }
        let control_count = self
            .forms
            .iter()
            .try_fold(self.unowned_controls.len(), |count, form| {
                count.checked_add(form.controls.len())
            })
            .ok_or_else(|| "form snapshot control count overflow".to_owned())?;
        if control_count > MAX_FORM_SNAPSHOT_CONTROLS {
            return Err("form snapshot exceeds its control bound".into());
        }

        let mut form_ids = BTreeSet::new();
        let mut control_ids = BTreeSet::new();
        for form in &self.forms {
            validate_id(&form.id, "form")?;
            if !form_ids.insert(form.id.as_str()) {
                return Err("form snapshot contains duplicate form IDs".into());
            }
            validate_optional_text(&form.accessible_name, MAX_FORM_TEXT_BYTES, "form name")?;
            validate_optional_text(&form.description, MAX_FORM_TEXT_BYTES, "form description")?;
            validate_target(form.target.as_ref())?;
            validate_form(form, &mut control_ids)?;
        }
        for control in &self.unowned_controls {
            if control.form_id.is_some() || control.group_id.is_some() {
                return Err("unowned controls cannot reference a form or group".into());
            }
            validate_control(control)?;
            if !control_ids.insert(control.id.as_str()) {
                return Err("form snapshot contains duplicate control IDs".into());
            }
        }
        Ok(())
    }
}

fn validate_form<'a>(
    form: &'a FormDescriptor,
    all_control_ids: &mut BTreeSet<&'a str>,
) -> Result<(), String> {
    if form.groups.len() > MAX_FORM_GROUPS || form.controls.len() > MAX_FORM_SNAPSHOT_CONTROLS {
        return Err("form exceeds a collection bound".into());
    }
    let mut local_controls = BTreeSet::new();
    for control in &form.controls {
        if control.form_id.as_deref() != Some(form.id.as_str()) {
            return Err("form control references the wrong form".into());
        }
        validate_control(control)?;
        if !local_controls.insert(control.id.as_str())
            || !all_control_ids.insert(control.id.as_str())
        {
            return Err("form snapshot contains duplicate control IDs".into());
        }
    }

    let mut group_ids = BTreeSet::new();
    for group in &form.groups {
        validate_id(&group.id, "form group")?;
        validate_optional_text(&group.label, MAX_FORM_TEXT_BYTES, "form group label")?;
        validate_optional_text(
            &group.description,
            MAX_FORM_TEXT_BYTES,
            "form group description",
        )?;
        if !group_ids.insert(group.id.as_str()) || group.control_ids.len() > MAX_FORM_REFERENCES {
            return Err("form contains invalid group references".into());
        }
        for id in &group.control_ids {
            if !local_controls.contains(id.as_str()) {
                return Err("form group references an unknown control".into());
            }
            let control = form
                .controls
                .iter()
                .find(|control| control.id == *id)
                .unwrap();
            if control.group_id.as_deref() != Some(group.id.as_str()) {
                return Err("form group membership is inconsistent".into());
            }
        }
    }
    for control in &form.controls {
        if let Some(group_id) = &control.group_id {
            if !group_ids.contains(group_id.as_str()) {
                return Err("form control references an unknown group".into());
            }
            let group = form
                .groups
                .iter()
                .find(|group| group.id == *group_id)
                .unwrap();
            if !group.control_ids.iter().any(|id| id == &control.id) {
                return Err("form group membership is inconsistent".into());
            }
        }
    }
    validate_control_references(form, &local_controls)?;
    Ok(())
}

fn validate_control_references(
    form: &FormDescriptor,
    controls: &BTreeSet<&str>,
) -> Result<(), String> {
    if form.submit_control_ids.len() > MAX_FORM_REFERENCES
        || form.reset_control_ids.len() > MAX_FORM_REFERENCES
        || form.validity.invalid_control_ids.len() > MAX_FORM_REFERENCES
    {
        return Err("form exceeds its control reference bound".into());
    }
    if !references_are_unique(&form.submit_control_ids)
        || !references_are_unique(&form.reset_control_ids)
        || !references_are_unique(&form.validity.invalid_control_ids)
    {
        return Err("form contains duplicate control references".into());
    }
    for id in &form.submit_control_ids {
        let Some(control) = form.controls.iter().find(|control| control.id == *id) else {
            return Err("form submit list references an unknown control".into());
        };
        if control.control_kind != FormControlKind::Submit {
            return Err("form submit list references a non-submit control".into());
        }
    }
    for id in &form.reset_control_ids {
        let Some(control) = form.controls.iter().find(|control| control.id == *id) else {
            return Err("form reset list references an unknown control".into());
        };
        if control.control_kind != FormControlKind::Reset {
            return Err("form reset list references a non-reset control".into());
        }
    }
    for id in &form.validity.invalid_control_ids {
        if !controls.contains(id.as_str()) {
            return Err("form validity references an unknown control".into());
        }
    }
    Ok(())
}

fn references_are_unique(values: &[String]) -> bool {
    values.iter().collect::<BTreeSet<_>>().len() == values.len()
}

fn validate_control(control: &FormControl) -> Result<(), String> {
    validate_id(&control.id, "form control")?;
    if let Some(id) = &control.form_id {
        validate_id(id, "form")?;
    }
    if let Some(id) = &control.group_id {
        validate_id(id, "form group")?;
    }
    validate_target(control.target.as_ref())?;
    validate_optional_text(
        &control.accessible_name,
        MAX_FORM_TEXT_BYTES,
        "control name",
    )?;
    validate_optional_text(&control.label, MAX_FORM_TEXT_BYTES, "control label")?;
    validate_optional_text(
        &control.description,
        MAX_FORM_TEXT_BYTES,
        "control description",
    )?;
    validate_optional_text(
        &control.placeholder,
        MAX_FORM_TEXT_BYTES,
        "control placeholder",
    )?;
    validate_optional_text(
        &control.autocomplete,
        MAX_FORM_TEXT_BYTES,
        "control autocomplete",
    )?;
    validate_state(control.control_kind, &control.state)?;
    validate_constraints(&control.constraints)?;
    validate_validity(&control.validity)?;
    if control.options.len() > MAX_FORM_OPTIONS {
        return Err("form control exceeds its option bound".into());
    }
    for option in &control.options {
        validate_text(&option.value, MAX_FORM_VALUE_BYTES, "option value", true)?;
        validate_text(&option.label, MAX_FORM_TEXT_BYTES, "option label", true)?;
        validate_optional_text(&option.group_label, MAX_FORM_TEXT_BYTES, "option group")?;
    }
    let operations = control
        .supported_operations
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if operations.len() != control.supported_operations.len() {
        return Err("form control contains duplicate supported operations".into());
    }
    Ok(())
}

fn validate_state(kind: FormControlKind, state: &FormControlState) -> Result<(), String> {
    match state {
        FormControlState::Text { value } => {
            if kind == FormControlKind::Password {
                return Err("password controls cannot expose text state".into());
            }
            validate_text(value, MAX_FORM_VALUE_BYTES, "control value", true)?;
        }
        FormControlState::Selection { values } => {
            if values.len() > MAX_FORM_OPTIONS {
                return Err("selection state exceeds its value bound".into());
            }
            for value in values {
                validate_text(value, MAX_FORM_VALUE_BYTES, "selection value", true)?;
            }
        }
        FormControlState::Files { count } if *count > MAX_FORM_OPTIONS => {
            return Err("file state exceeds its count bound".into());
        }
        _ => {}
    }
    Ok(())
}

fn validate_constraints(constraints: &FormControlConstraints) -> Result<(), String> {
    validate_optional_text(&constraints.pattern, MAX_FORM_TEXT_BYTES, "control pattern")?;
    validate_optional_text(&constraints.min, MAX_FORM_VALUE_BYTES, "control minimum")?;
    validate_optional_text(&constraints.max, MAX_FORM_VALUE_BYTES, "control maximum")?;
    validate_optional_text(&constraints.step, MAX_FORM_VALUE_BYTES, "control step")?;
    if constraints.accept.len() > MAX_FORM_ACCEPT_TYPES {
        return Err("control accept list exceeds its bound".into());
    }
    for accept in &constraints.accept {
        validate_text(accept, MAX_FORM_TEXT_BYTES, "accepted type", false)?;
    }
    if constraints
        .min_length
        .zip(constraints.max_length)
        .is_some_and(|(min, max)| min > max)
    {
        return Err("control minimum length exceeds maximum length".into());
    }
    Ok(())
}

fn validate_validity(validity: &FormControlValidity) -> Result<(), String> {
    if validity.flags.len() > 10 || validity.described_by.len() > MAX_FORM_REFERENCES {
        return Err("control validity exceeds a collection bound".into());
    }
    if validity.valid && !validity.flags.is_empty() {
        return Err("valid control cannot carry failing validity flags".into());
    }
    let flags = validity.flags.iter().copied().collect::<BTreeSet<_>>();
    if flags.len() != validity.flags.len() {
        return Err("control validity contains duplicate flags".into());
    }
    validate_optional_text(
        &validity.message,
        MAX_FORM_VALIDATION_MESSAGE_BYTES,
        "validation message",
    )?;
    for text in &validity.described_by {
        validate_text(text, MAX_FORM_TEXT_BYTES, "described-by text", false)?;
    }
    Ok(())
}

fn validate_target(target: Option<&FormControlTarget>) -> Result<(), String> {
    let Some(target) = target else {
        return Ok(());
    };
    validate_text(&target.role, MAX_FORM_ID_BYTES, "target role", false)?;
    validate_text(
        &target.accessible_name,
        MAX_FORM_TEXT_BYTES,
        "target accessible name",
        false,
    )?;
    if target
        .ordinal
        .is_some_and(|ordinal| ordinal > MAX_FORM_TARGET_ORDINAL)
        || target.frame_path.len() > MAX_FORM_TARGET_PATH
        || target.shadow_path.len() > MAX_FORM_TARGET_PATH
    {
        return Err("form target exceeds its bound".into());
    }
    for segment in target.frame_path.iter().chain(&target.shadow_path) {
        validate_text(&segment.role, MAX_FORM_ID_BYTES, "target path role", false)?;
        validate_text(
            &segment.accessible_name,
            MAX_FORM_TEXT_BYTES,
            "target path name",
            false,
        )?;
        if segment
            .ordinal
            .is_some_and(|ordinal| ordinal > MAX_FORM_TARGET_ORDINAL)
        {
            return Err("form target path ordinal exceeds its bound".into());
        }
    }
    Ok(())
}

fn validate_id(value: &str, field: &str) -> Result<(), String> {
    validate_text(value, MAX_FORM_ID_BYTES, field, false)
}

fn validate_optional_text(value: &Option<String>, max: usize, field: &str) -> Result<(), String> {
    if let Some(value) = value {
        validate_text(value, max, field, false)?;
    }
    Ok(())
}

fn validate_text(value: &str, max: usize, field: &str, allow_empty: bool) -> Result<(), String> {
    if (!allow_empty && value.is_empty())
        || value.len() > max
        || value.chars().any(char::is_control)
    {
        return Err(format!(
            "{field} is empty, oversized, or contains control characters"
        ));
    }
    Ok(())
}
