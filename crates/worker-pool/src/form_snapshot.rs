use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use types::{
    CommandError, ControlAction, ControlActionEvidence, ErrorCode, ErrorLayer, FormControl,
    FormControlConstraints, FormControlKind, FormControlOperation, FormControlState,
    FormControlTarget, FormControlValidity, FormDescriptor, FormGroup, FormOption, FormSnapshot,
    FormValidity, FormValidityFlag, PageId, SemanticTargetSegment, FORM_SNAPSHOT_SCHEMA_VERSION,
    MAX_FORM_SNAPSHOT_CONTROLS,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RawFormSnapshot {
    pub forms: Vec<RawForm>,
    pub groups: Vec<RawFormGroup>,
    pub controls: Vec<RawFormControl>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RawForm {
    pub key: String,
    pub accessible_name: Option<String>,
    pub description: Option<String>,
    pub frame_path: Vec<SemanticTargetSegment>,
    pub shadow_path: Vec<SemanticTargetSegment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RawFormGroup {
    pub key: String,
    pub form_key: String,
    pub label: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RawFormOption {
    pub value: String,
    pub label: String,
    pub disabled: bool,
    pub selected: bool,
    pub group_label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RawFormControl {
    pub key: String,
    pub form_key: Option<String>,
    pub group_key: Option<String>,
    pub tag: String,
    pub input_type: Option<String>,
    pub content_editable: bool,
    pub explicit_role: Option<String>,
    pub accessible_name: Option<String>,
    pub label: Option<String>,
    pub description: Option<String>,
    pub placeholder: Option<String>,
    pub autocomplete: Option<String>,
    pub value: Option<String>,
    pub value_present: bool,
    pub checked: bool,
    pub file_count: usize,
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
    pub will_validate: bool,
    pub valid: bool,
    pub validity_flags: Vec<FormValidityFlag>,
    pub validation_message: Option<String>,
    pub described_by: Vec<String>,
    pub options: Vec<RawFormOption>,
    pub frame_path: Vec<SemanticTargetSegment>,
    pub shadow_path: Vec<SemanticTargetSegment>,
}

pub fn normalize_form_snapshot(
    page_id: PageId,
    raw: RawFormSnapshot,
    max_controls: usize,
) -> Result<FormSnapshot, CommandError> {
    if !(1..=MAX_FORM_SNAPSHOT_CONTROLS).contains(&max_controls) {
        return Err(snapshot_error(
            ErrorCode::InvalidRequest,
            "maxControls must be between 1 and 512",
        ));
    }

    let raw_control_count = raw.controls.len();
    let retained = raw
        .controls
        .into_iter()
        .take(max_controls)
        .collect::<Vec<_>>();
    let truncated = raw.truncated || retained.len() < raw_control_count;
    let mut form_ids = BTreeMap::new();
    for (index, form) in raw.forms.iter().enumerate() {
        form_ids.insert(form.key.clone(), format!("form-{}", index + 1));
    }
    let mut group_ids = BTreeMap::new();
    for (index, group) in raw.groups.iter().enumerate() {
        if form_ids.contains_key(&group.form_key) {
            group_ids.insert(group.key.clone(), format!("group-{}", index + 1));
        }
    }
    let control_ids = retained
        .iter()
        .enumerate()
        .map(|(index, control)| (control.key.clone(), format!("control-{}", index + 1)))
        .collect::<BTreeMap<_, _>>();

    let mut target_totals = BTreeMap::new();
    for control in &retained {
        if let (Some(role), Some(name)) = (control_role(control), control.accessible_name.as_ref())
        {
            if !name.is_empty() {
                *target_totals.entry((role, name.clone())).or_insert(0usize) += 1;
            }
        }
    }
    let mut target_seen = BTreeMap::new();
    let mut normalized = Vec::with_capacity(retained.len());
    for control in retained {
        let id = control_ids[&control.key].clone();
        normalized.push(normalize_control(
            control,
            id,
            &form_ids,
            &group_ids,
            &target_totals,
            &mut target_seen,
        ));
    }

    let mut forms = Vec::new();
    for form in raw.forms {
        let Some(form_id) = form_ids.get(&form.key).cloned() else {
            continue;
        };
        let controls = normalized
            .iter()
            .filter(|control| control.form_id.as_deref() == Some(form_id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if controls.is_empty() {
            continue;
        }
        let groups = raw
            .groups
            .iter()
            .filter(|group| group.form_key == form.key)
            .filter_map(|group| {
                let id = group_ids.get(&group.key)?.clone();
                let control_ids = controls
                    .iter()
                    .filter(|control| control.group_id.as_deref() == Some(id.as_str()))
                    .map(|control| control.id.clone())
                    .collect::<Vec<_>>();
                (!control_ids.is_empty()).then(|| FormGroup {
                    id,
                    label: group.label.clone(),
                    description: group.description.clone(),
                    control_ids,
                })
            })
            .collect::<Vec<_>>();
        let invalid_control_ids = controls
            .iter()
            .filter(|control| !control.validity.valid)
            .map(|control| control.id.clone())
            .collect::<Vec<_>>();
        forms.push(FormDescriptor {
            id: form_id,
            target: form.accessible_name.as_ref().map(|name| FormControlTarget {
                role: "form".into(),
                accessible_name: name.clone(),
                ordinal: None,
                frame_path: form.frame_path,
                shadow_path: form.shadow_path,
            }),
            accessible_name: form.accessible_name,
            description: form.description,
            groups,
            submit_control_ids: controls
                .iter()
                .filter(|control| control.control_kind == FormControlKind::Submit)
                .map(|control| control.id.clone())
                .collect(),
            reset_control_ids: controls
                .iter()
                .filter(|control| control.control_kind == FormControlKind::Reset)
                .map(|control| control.id.clone())
                .collect(),
            validity: FormValidity {
                valid: invalid_control_ids.is_empty(),
                invalid_control_ids,
            },
            controls,
        });
    }
    let owned_ids = forms
        .iter()
        .flat_map(|form| form.controls.iter().map(|control| control.id.as_str()))
        .collect::<BTreeSet<_>>();
    let unowned_controls = normalized
        .into_iter()
        .filter(|control| !owned_ids.contains(control.id.as_str()))
        .map(|mut control| {
            control.form_id = None;
            control.group_id = None;
            control
        })
        .collect();
    let snapshot = FormSnapshot {
        schema_version: FORM_SNAPSHOT_SCHEMA_VERSION,
        page_id,
        forms,
        unowned_controls,
        truncated,
    };
    snapshot
        .validate()
        .map_err(|message| snapshot_error(ErrorCode::BrowserCommandFailed, message))?;
    Ok(snapshot)
}

pub fn decode_form_snapshot(
    page_id: PageId,
    encoded: &str,
    max_controls: usize,
) -> Result<FormSnapshot, CommandError> {
    let raw: RawFormSnapshot = serde_json::from_str(encoded).map_err(|error| {
        snapshot_error(
            ErrorCode::BrowserCommandFailed,
            format!("invalid raw form snapshot: {error}"),
        )
    })?;
    normalize_form_snapshot(page_id, raw, max_controls)
}

fn normalize_control(
    raw: RawFormControl,
    id: String,
    form_ids: &BTreeMap<String, String>,
    group_ids: &BTreeMap<String, String>,
    target_totals: &BTreeMap<(String, String), usize>,
    target_seen: &mut BTreeMap<(String, String), usize>,
) -> FormControl {
    let kind = control_kind(&raw);
    let role = control_role(&raw);
    let target = role
        .zip(raw.accessible_name.clone())
        .and_then(|(role, name)| {
            if name.is_empty() {
                return None;
            }
            let key = (role.clone(), name.clone());
            let seen = target_seen.entry(key.clone()).or_default();
            let ordinal =
                (target_totals.get(&key).copied().unwrap_or_default() > 1).then_some(*seen);
            *seen += 1;
            Some(FormControlTarget {
                role,
                accessible_name: name,
                ordinal,
                frame_path: raw.frame_path.clone(),
                shadow_path: raw.shadow_path.clone(),
            })
        });
    let state = match kind {
        FormControlKind::Password => FormControlState::Redacted {
            present: raw.value_present,
        },
        FormControlKind::Checkbox | FormControlKind::Radio | FormControlKind::Switch => {
            FormControlState::Checked {
                checked: raw.checked,
            }
        }
        FormControlKind::SelectOne | FormControlKind::SelectMultiple | FormControlKind::Listbox => {
            FormControlState::Selection {
                values: raw
                    .options
                    .iter()
                    .filter(|option| option.selected)
                    .map(|option| option.value.clone())
                    .collect(),
            }
        }
        FormControlKind::File => FormControlState::Files {
            count: raw.file_count,
        },
        _ => raw
            .value
            .clone()
            .filter(|value| !value.is_empty())
            .map(|value| FormControlState::Text { value })
            .unwrap_or(FormControlState::Empty),
    };
    let supported_operations = supported_operations(
        kind,
        raw.disabled,
        raw.read_only,
        &raw.tag,
        raw.content_editable,
    );
    FormControl {
        id,
        form_id: raw
            .form_key
            .as_ref()
            .and_then(|key| form_ids.get(key))
            .cloned(),
        group_id: raw
            .group_key
            .as_ref()
            .and_then(|key| group_ids.get(key))
            .cloned(),
        target,
        control_kind: kind,
        accessible_name: raw.accessible_name,
        label: raw.label,
        description: raw.description,
        placeholder: raw.placeholder,
        autocomplete: raw.autocomplete,
        state,
        constraints: FormControlConstraints {
            required: raw.required,
            read_only: raw.read_only,
            disabled: raw.disabled,
            pattern: raw.pattern,
            min_length: raw.min_length,
            max_length: raw.max_length,
            min: raw.min,
            max: raw.max,
            step: raw.step,
            multiple: raw.multiple,
            accept: raw.accept,
        },
        validity: FormControlValidity {
            will_validate: raw.will_validate,
            valid: raw.valid,
            flags: raw.validity_flags,
            message: raw.validation_message,
            described_by: raw.described_by,
        },
        options: raw
            .options
            .into_iter()
            .map(|option| FormOption {
                value: option.value,
                label: option.label,
                disabled: option.disabled,
                selected: option.selected,
                group_label: option.group_label,
            })
            .collect(),
        supported_operations,
    }
}

fn control_kind(control: &RawFormControl) -> FormControlKind {
    match control.explicit_role.as_deref() {
        Some("switch") => return FormControlKind::Switch,
        Some("combobox") => return FormControlKind::Combobox,
        Some("listbox") => return FormControlKind::Listbox,
        _ => {}
    }
    if control.content_editable {
        return FormControlKind::ContentEditable;
    }
    if control.tag.eq_ignore_ascii_case("select") {
        return if control.multiple {
            FormControlKind::SelectMultiple
        } else {
            FormControlKind::SelectOne
        };
    }
    if control.tag.eq_ignore_ascii_case("textarea") {
        return FormControlKind::Text;
    }
    match control.input_type.as_deref().unwrap_or("text") {
        "email" => FormControlKind::Email,
        "password" => FormControlKind::Password,
        "search" => FormControlKind::Search,
        "number" => FormControlKind::Number,
        "checkbox" => FormControlKind::Checkbox,
        "radio" => FormControlKind::Radio,
        "date" => FormControlKind::Date,
        "time" => FormControlKind::Time,
        "datetime-local" => FormControlKind::DateTimeLocal,
        "range" => FormControlKind::Range,
        "file" => FormControlKind::File,
        "submit" => FormControlKind::Submit,
        "reset" => FormControlKind::Reset,
        "text" => FormControlKind::Text,
        _ if control.tag.eq_ignore_ascii_case("button") => FormControlKind::Other,
        _ => FormControlKind::Other,
    }
}

fn control_role(control: &RawFormControl) -> Option<String> {
    control.explicit_role.clone().or_else(|| {
        Some(
            match control_kind(control) {
                FormControlKind::Checkbox => "checkbox",
                FormControlKind::Radio => "radio",
                FormControlKind::Switch => "switch",
                FormControlKind::SelectOne | FormControlKind::Combobox => "combobox",
                FormControlKind::SelectMultiple | FormControlKind::Listbox => "listbox",
                FormControlKind::Range => "slider",
                FormControlKind::Number => "spinbutton",
                FormControlKind::Search => "searchbox",
                FormControlKind::Submit | FormControlKind::Reset | FormControlKind::Other => {
                    "button"
                }
                FormControlKind::File => "button",
                _ => "textbox",
            }
            .into(),
        )
    })
}

fn supported_operations(
    kind: FormControlKind,
    disabled: bool,
    read_only: bool,
    tag: &str,
    content_editable: bool,
) -> Vec<FormControlOperation> {
    if disabled || read_only {
        return Vec::new();
    }
    match kind {
        FormControlKind::Text
        | FormControlKind::Email
        | FormControlKind::Password
        | FormControlKind::Search
        | FormControlKind::Number
        | FormControlKind::Date
        | FormControlKind::Time
        | FormControlKind::DateTimeLocal
        | FormControlKind::Range
        | FormControlKind::ContentEditable => {
            vec![FormControlOperation::SetText, FormControlOperation::Clear]
        }
        FormControlKind::Checkbox | FormControlKind::Radio | FormControlKind::Switch => {
            vec![FormControlOperation::SetChecked]
        }
        FormControlKind::Combobox
            if content_editable
                || tag.eq_ignore_ascii_case("input")
                || tag.eq_ignore_ascii_case("textarea") =>
        {
            vec![FormControlOperation::SetText, FormControlOperation::Clear]
        }
        FormControlKind::SelectOne | FormControlKind::Combobox => {
            vec![FormControlOperation::SelectOne, FormControlOperation::Clear]
        }
        FormControlKind::SelectMultiple | FormControlKind::Listbox => {
            vec![
                FormControlOperation::SelectMany,
                FormControlOperation::Clear,
            ]
        }
        FormControlKind::File => vec![FormControlOperation::SetFiles, FormControlOperation::Clear],
        FormControlKind::Submit | FormControlKind::Reset | FormControlKind::Other => {
            vec![FormControlOperation::Activate]
        }
    }
}

/// Semantic target equality for control lookup. Snapshots omit `ordinal`
/// when a role/name pair is unique while callers may pass `ordinal: 0`
/// explicitly (a11y targets do); both mean "the first match", so a struct
/// compare false-rejects a target copied from a different snapshot.
pub fn target_specs_equivalent(a: &types::FormControlTarget, b: &types::FormControlTarget) -> bool {
    a.ordinal.unwrap_or(0) == b.ordinal.unwrap_or(0)
        && a.role.eq_ignore_ascii_case(&b.role)
        && a.accessible_name == b.accessible_name
        && a.frame_path == b.frame_path
        && a.shadow_path == b.shadow_path
}

pub fn validate_control_action(
    control: &FormControl,
    action: &ControlAction,
) -> Result<(), CommandError> {
    action
        .validate()
        .map_err(|message| snapshot_error(ErrorCode::InvalidRequest, message))?;
    let operation = action.operation();
    if !control.supported_operations.contains(&operation) {
        return Err(snapshot_error(
            ErrorCode::IntentActionMismatch,
            format!(
                "control kind {:?} does not support operation {:?}",
                control.control_kind, operation
            ),
        ));
    }
    if control.constraints.disabled || control.constraints.read_only {
        return Err(snapshot_error(
            ErrorCode::IntentActionMismatch,
            "control is not mutable",
        ));
    }
    Ok(())
}

/// `committed`: the option values the driver actually selected, when the
/// action resolved them — selection requests may name an option by *label*
/// (snapshots surface labels), while the post-action snapshot state carries
/// option *values*, so verifying against the requested string false-fails
/// whenever the two differ.
pub fn control_action_evidence(
    control: &FormControl,
    action: &ControlAction,
    node_replaced: bool,
    committed: Option<&[String]>,
) -> Result<ControlActionEvidence, CommandError> {
    validate_control_action(control, action)?;
    let matched = match (action, &control.state) {
        (ControlAction::SetText { value }, FormControlState::Text { value: actual }) => {
            value == actual
        }
        (ControlAction::SetText { value }, FormControlState::Empty) => value.is_empty(),
        (ControlAction::SetText { value }, FormControlState::Redacted { present }) => {
            *present != value.is_empty()
        }
        (ControlAction::SetChecked { checked }, FormControlState::Checked { checked: actual }) => {
            checked == actual
        }
        (ControlAction::SelectOne { value }, FormControlState::Selection { values }) => {
            match committed {
                Some(committed) => values == committed,
                None => values.len() == 1 && values[0] == *value,
            }
        }
        (ControlAction::SelectMany { values }, FormControlState::Selection { values: actual }) => {
            match committed {
                Some(committed) => {
                    committed.iter().collect::<BTreeSet<_>>()
                        == actual.iter().collect::<BTreeSet<_>>()
                }
                None => {
                    values.iter().collect::<BTreeSet<_>>() == actual.iter().collect::<BTreeSet<_>>()
                }
            }
        }
        (ControlAction::SetFiles { paths }, FormControlState::Files { count }) => {
            paths.len() == *count
        }
        (ControlAction::Clear, FormControlState::Empty) => true,
        (ControlAction::Clear, FormControlState::Text { value }) => value.is_empty(),
        (ControlAction::Clear, FormControlState::Redacted { present }) => !present,
        (ControlAction::Clear, FormControlState::Checked { checked }) => !checked,
        (ControlAction::Clear, FormControlState::Selection { values }) => values.is_empty(),
        (ControlAction::Clear, FormControlState::Files { count }) => *count == 0,
        (ControlAction::Activate, _) => true,
        _ => false,
    };
    if !matched {
        return Err(snapshot_error(
            ErrorCode::VerificationFailed,
            "control action immediate postcondition was not observed",
        ));
    }
    let target = control.target.clone().ok_or_else(|| {
        snapshot_error(
            ErrorCode::TargetNotFound,
            "control has no stable semantic target",
        )
    })?;
    Ok(ControlActionEvidence {
        operation: action.operation(),
        target,
        state: control.state.clone(),
        validity: control.validity.clone(),
        node_replaced,
    })
}

fn snapshot_error(code: ErrorCode, message: impl Into<String>) -> CommandError {
    CommandError {
        code,
        message: message.into(),
        layer: ErrorLayer::Browser,
        retryable: false,
    }
}

/// Returns a self-contained, read-only DOM projection. The result is JSON text so
/// Chromium CDP and Firefox BiDi apply identical serialization semantics.
pub fn form_snapshot_expression(page_id: &PageId) -> String {
    form_snapshot_expression_with_limit(page_id, MAX_FORM_SNAPSHOT_CONTROLS)
}

pub fn form_snapshot_expression_with_limit(_page_id: &PageId, max_controls: usize) -> String {
    RAW_FORM_SNAPSHOT_SCRIPT.replace("__MAX_CONTROLS__", &max_controls.to_string())
}

const RAW_FORM_SNAPSHOT_SCRIPT: &str = r#"JSON.stringify((()=>{
const LIMIT=Math.max(1,Math.min(512,__MAX_CONTROLS__));
const clip=(v,n)=>{let s=String(v??'').replace(/[\u0000-\u001f\u007f-\u009f]/gu,' ');const enc=new TextEncoder();let bytes=enc.encode(s).length;while(bytes>n&&s){s=s.slice(0,Math.max(0,Math.floor(s.length*n/bytes)));bytes=enc.encode(s).length}return s};
const text=(v,n=2048)=>{const s=clip(v,n).trim().replace(/\s+/gu,' ');return s||null};
const roots=[];const scanRoot=(root,framePath=[],shadowPath=[])=>{roots.push({root,framePath,shadowPath});for(const el of root.querySelectorAll('*')){if(el.shadowRoot)scanRoot(el.shadowRoot,framePath,[...shadowPath,{role:el.getAttribute('role')||'group',accessibleName:text(el.getAttribute('aria-label'))||text(el.id)||'shadow root',ordinal:null}]);if(el.tagName==='IFRAME'){try{if(el.contentDocument)scanRoot(el.contentDocument,[...framePath,{role:'iframe',accessibleName:text(el.getAttribute('aria-label'))||text(el.name)||text(el.title)||'frame',ordinal:null}],shadowPath)}catch{}}}};scanRoot(document);
const formElements=[];for(const {root} of roots)for(const form of root.querySelectorAll('form'))if(!formElements.includes(form))formElements.push(form);let truncated=formElements.length>64;formElements.splice(64);
const formKeys=new Map(formElements.map((form,index)=>[form,`raw-form-${index+1}`]));
const groupElements=[];for(const form of formElements)for(const group of form.querySelectorAll('fieldset'))if(groupElements.length<128)groupElements.push(group);else truncated=true;
const groupKeys=new Map(groupElements.map((group,index)=>[group,`raw-group-${index+1}`]));
const controlElements=[];for(const {root,framePath,shadowPath} of roots){for(const element of root.querySelectorAll('input,textarea,select,button,[contenteditable="true"],[role="switch"],[role="combobox"],[role="listbox"]')){if(controlElements.length>=LIMIT){truncated=true;break}if(!controlElements.some(item=>item.element===element))controlElements.push({element,framePath,shadowPath})}}
const resolveRefs=(element,attribute)=>{const ids=(element.getAttribute(attribute)||'').trim().split(/\s+/).filter(Boolean).slice(0,512);return ids.map(id=>text(element.ownerDocument.getElementById(id)?.textContent)).filter(Boolean)};
const label=(element)=>text(element.getAttribute('aria-label'))||text(resolveRefs(element,'aria-labelledby').join(' '))||text(element.labels?.[0]?.textContent)||text(element.closest('label')?.textContent)||text(element.getAttribute('placeholder'));
const forms=formElements.map((form,index)=>{const scope=roots.find(item=>item.root===form.getRootNode())||{framePath:[],shadowPath:[]};return{key:formKeys.get(form),accessibleName:text(form.getAttribute('aria-label'))||text(resolveRefs(form,'aria-labelledby').join(' '))||text(form.getAttribute('name')),description:text(resolveRefs(form,'aria-describedby').join(' '))||text(form.getAttribute('aria-description')),framePath:scope.framePath,shadowPath:scope.shadowPath}});
const groups=groupElements.map(group=>({key:groupKeys.get(group),formKey:formKeys.get(group.closest('form')),label:text(group.querySelector(':scope > legend')?.textContent),description:text(resolveRefs(group,'aria-describedby').join(' '))||text(group.getAttribute('aria-description'))})).filter(group=>group.formKey);
const controls=controlElements.map(({element:e,framePath,shadowPath},index)=>{const owner=e.form&&formKeys.get(e.form)||null;const fieldset=e.closest('fieldset');const groupKey=fieldset&&groupKeys.get(fieldset)||null;const tag=e.tagName.toLowerCase();const type=text(e.getAttribute('type'))||('type'in e?text(e.type):null);const isPassword=type==='password';const describedBy=resolveRefs(e,'aria-describedby');const validity=e.validity||{};const validityFlags=['valueMissing','typeMismatch','patternMismatch','tooLong','tooShort','rangeUnderflow','rangeOverflow','stepMismatch','badInput','customError'].filter(name=>validity[name]);const options=[];if(e.options)for(const option of [...e.options].slice(0,512)){const group=option.parentElement?.tagName==='OPTGROUP'?option.parentElement:null;options.push({value:clip(option.value,4096),label:clip(option.label||option.textContent||'',2048),disabled:Boolean(option.disabled||group?.disabled),selected:Boolean(option.selected),groupLabel:text(group?.label)})}if(e.options?.length>512)truncated=true;return{key:`raw-control-${index+1}`,formKey:owner,groupKey,tag,inputType:type,contentEditable:Boolean(e.isContentEditable),explicitRole:text(e.getAttribute('role')),accessibleName:label(e),label:text(e.labels?.[0]?.textContent)||text(e.closest('label')?.textContent),description:text(describedBy.join(' '))||text(e.getAttribute('aria-description')),placeholder:text(e.getAttribute('placeholder')),autocomplete:text(e.getAttribute('autocomplete')),value:isPassword?null:('value'in e?clip(e.value,4096):clip(e.textContent||'',4096)),valuePresent:Boolean('value'in e?e.value:e.textContent),checked:Boolean(e.checked||e.getAttribute('aria-checked')==='true'),fileCount:type==='file'?Math.min(e.files?.length||0,512):0,required:Boolean(e.required||e.getAttribute('aria-required')==='true'),readOnly:Boolean(e.readOnly||e.getAttribute('aria-readonly')==='true'),disabled:Boolean(e.matches(':disabled')||e.getAttribute('aria-disabled')==='true'||e.closest('[inert]')),pattern:text(e.getAttribute('pattern')),minLength:Number.isInteger(e.minLength)&&e.minLength>=0?e.minLength:null,maxLength:Number.isInteger(e.maxLength)&&e.maxLength>=0?e.maxLength:null,min:text(e.getAttribute('min')),max:text(e.getAttribute('max')),step:text(e.getAttribute('step')),multiple:Boolean(e.multiple||e.getAttribute('aria-multiselectable')==='true'),accept:(e.getAttribute('accept')||'').split(',').map(value=>text(value)).filter(Boolean).slice(0,128),willValidate:Boolean(e.willValidate),valid:validity.valid!==false,validityFlags,validationMessage:isPassword?null:text(e.validationMessage,1024),describedBy,options,framePath,shadowPath}});
return{forms,groups,controls,truncated};})())"#;

const _LEGACY_FORM_SNAPSHOT_SCRIPT: &str = r#"JSON.stringify((()=>{
const clip=(v,n)=>{let s=String(v??'').replace(/[\u0000-\u001f\u007f-\u009f]/gu,' ');const enc=new TextEncoder();let bytes=enc.encode(s).length;while(bytes>n&&s){s=s.slice(0,Math.max(0,Math.floor(s.length*n/bytes)));bytes=enc.encode(s).length}return s};
const text=(v,n=2048)=>{const s=clip(v,n).trim().replace(/\s+/gu,' ');return s||null};
const controls=[...document.querySelectorAll('input,textarea,select,button,[contenteditable="true"],[role="switch"],[role="combobox"],[role="listbox"]')];
const forms=[...document.forms].slice(0,64);const formIds=new Map(forms.map((e,i)=>[e,`form-${i}`]));
const eligibleControls=controls.filter(e=>!e.form||formIds.has(e.form));let truncated=document.forms.length>64||eligibleControls.length>512||eligibleControls.length!==controls.length;let optionCount=0;
const visibleControls=eligibleControls.slice(0,512);const controlIds=new Map(visibleControls.map((e,i)=>[e,`control-${i}`]));
const label=e=>text(e.getAttribute('aria-label'))||text((e.getAttribute('aria-labelledby')||'').split(/\s+/).filter(Boolean).map(id=>document.getElementById(id)?.textContent||'').join(' '))||text(e.labels?.[0]?.textContent)||text(e.closest('label')?.textContent)||text(e.getAttribute('placeholder'));
const role=e=>{const explicit=text(e.getAttribute('role'));if(explicit)return explicit;const tag=e.tagName.toLowerCase(),type=String(e.type||e.getAttribute('type')||'text').toLowerCase();if(tag==='button'||['submit','reset','button','file'].includes(type))return 'button';if(type==='checkbox')return 'checkbox';if(type==='radio')return 'radio';if(tag==='select')return e.multiple?'listbox':'combobox';if(tag==='textarea'||e.isContentEditable)return 'textbox';return type==='range'?'slider':'textbox'};
const kind=e=>{const tag=e.tagName.toLowerCase(),type=String(e.type||e.getAttribute('type')||'text').toLowerCase(),r=e.getAttribute('role');if(r==='switch')return'switch';if(r==='combobox')return'combobox';if(r==='listbox')return'listbox';if(e.isContentEditable)return'contentEditable';if(tag==='button')return type==='reset'?'reset':type==='button'?'other':'submit';if(tag==='select')return e.multiple?'selectMultiple':'selectOne';if(tag==='textarea')return'text';return ({text:'text',email:'email',password:'password',search:'search',number:'number',checkbox:'checkbox',radio:'radio',date:'date',time:'time','datetime-local':'dateTimeLocal',range:'range',file:'file',submit:'submit',reset:'reset'})[type]||'other'};
const names=new Map();for(const e of visibleControls){const k=`${role(e)}\0${label(e)||''}`;names.set(k,(names.get(k)||0)+1)}const seen=new Map();
const describe=e=>{const k=kind(e),tag=e.tagName.toLowerCase(),editableCombo=k==='combobox'&&(tag==='input'||tag==='textarea'||e.isContentEditable),name=label(e),rk=`${role(e)}\0${name||''}`,ordinal=seen.get(rk)||0;seen.set(rk,ordinal+1);const target=name?{role:role(e),accessibleName:name,...(names.get(rk)>1?{ordinal}:{}),framePath:[],shadowPath:[]}:null;const selected=e.tagName==='SELECT'?[...e.selectedOptions].map(o=>clip(o.value,4096)):[];let state;if(k==='password')state={kind:'redacted',present:Boolean(e.value)};else if(k==='checkbox'||k==='radio'||k==='switch')state={kind:'checked',checked:Boolean(e.checked||e.getAttribute('aria-checked')==='true')};else if(k==='selectOne'||k==='selectMultiple'||k==='listbox'||(k==='combobox'&&!editableCombo))state={kind:'selection',values:selected};else if(k==='file'){const count=e.files?.length||0;if(count>512)truncated=true;state={kind:'files',count:Math.min(count,512)}}else state=e.value||e.textContent?{kind:'text',value:clip(e.value??e.textContent??'',4096)}:{kind:'empty'};
const v=e.validity||{},flags=[['valueMissing','valueMissing'],['typeMismatch','typeMismatch'],['patternMismatch','patternMismatch'],['tooLong','tooLong'],['tooShort','tooShort'],['rangeUnderflow','rangeUnderflow'],['rangeOverflow','rangeOverflow'],['stepMismatch','stepMismatch'],['badInput','badInput'],['customError','customError']].filter(([p])=>v[p]).map(([,n])=>n);const described=k==='password'?[]:(e.getAttribute('aria-describedby')||'').split(/\s+/).filter(Boolean).slice(0,512).map(id=>text(document.getElementById(id)?.textContent)).filter(Boolean);
let opts=[];if(e.options){for(const o of [...e.options]){if(optionCount++>=512){truncated=true;break}const group=o.parentElement?.tagName==='OPTGROUP'?o.parentElement:null;opts.push({value:clip(o.value,4096),label:clip(o.label||o.textContent||'',2048),disabled:Boolean(o.disabled||group?.disabled),selected:Boolean(o.selected),groupLabel:text(group?.label)})}}
let ops=['clear'];if(['text','email','password','search','number','date','time','dateTimeLocal','range','contentEditable'].includes(k)||editableCombo)ops.unshift('setText');else if(['checkbox','radio','switch'].includes(k))ops.unshift('setChecked');else if(['selectOne','combobox'].includes(k))ops.unshift('selectOne');else if(['selectMultiple','listbox'].includes(k))ops.unshift('selectMany');else if(k==='file')ops.unshift('setFiles');else if(['submit','reset'].includes(k)||tag==='button'||(tag==='input'&&e.type==='button'))ops=['activate'];
const owner=e.form&&formIds.has(e.form)?formIds.get(e.form):null,fieldset=e.closest('fieldset'),fieldsetIndex=fieldset&&owner?[...e.form.querySelectorAll('fieldset')].indexOf(fieldset):-1,minLength=Number.isInteger(e.minLength)&&e.minLength>=0?e.minLength:null,maxLength=Number.isInteger(e.maxLength)&&e.maxLength>=0?e.maxLength:null;return{id:controlIds.get(e),formId:owner,groupId:fieldsetIndex>=0?`${owner}-group-${fieldsetIndex}`:null,target,controlKind:k,accessibleName:name,label:text(e.labels?.[0]?.textContent),description:described[0]||null,placeholder:text(e.getAttribute('placeholder')),autocomplete:text(e.getAttribute('autocomplete')),state,constraints:{required:Boolean(e.required),readOnly:Boolean(e.readOnly),disabled:Boolean(e.matches(':disabled')||e.getAttribute('aria-disabled')==='true'),pattern:text(e.getAttribute('pattern')),minLength:minLength!==null&&maxLength!==null&&minLength>maxLength?null:minLength,maxLength,min:text(e.getAttribute('min')),max:text(e.getAttribute('max')),step:text(e.getAttribute('step')),multiple:Boolean(e.multiple),accept:(e.getAttribute('accept')||'').split(',').map(x=>text(x)).filter(Boolean).slice(0,128)},validity:{willValidate:Boolean(e.willValidate),valid:v.valid!==false,flags,message:k==='password'?null:text(e.validationMessage,1024),describedBy:described},options:opts,supportedOperations:ops}};
const all=visibleControls.map(describe);const descriptors=forms.map((f,i)=>{const id=formIds.get(f),owned=all.filter(c=>c.formId===id),fieldsets=[...f.querySelectorAll('fieldset')].slice(0,128);if(f.querySelectorAll('fieldset').length>128)truncated=true;const groups=fieldsets.map((g,j)=>({id:`${id}-group-${j}`,label:text(g.querySelector(':scope > legend')?.textContent),description:null,controlIds:owned.filter(c=>c.groupId===`${id}-group-${j}`).map(c=>c.id)}));const invalid=owned.filter(c=>!c.validity.valid).map(c=>c.id);return{id,target:null,accessibleName:text(f.getAttribute('aria-label'))||text(f.getAttribute('name')),description:null,groups,controls:owned,submitControlIds:owned.filter(c=>c.controlKind==='submit').map(c=>c.id),resetControlIds:owned.filter(c=>c.controlKind==='reset').map(c=>c.id),validity:{valid:invalid.length===0,invalidControlIds:invalid}}});
return{schemaVersion:1,pageId:__PAGE_ID__,forms:descriptors,unownedControls:all.filter(c=>!c.formId),truncated};})())"#;

#[cfg(test)]
mod tests {
    use super::*;
    use types::{
        ControlAction, FormControlKind, FormControlOperation, FormControlState, FormValidityFlag,
        SemanticTargetSegment,
    };

    fn raw_control(key: &str, name: &str) -> RawFormControl {
        RawFormControl {
            key: key.into(),
            form_key: None,
            group_key: None,
            tag: "input".into(),
            input_type: Some("text".into()),
            content_editable: false,
            explicit_role: None,
            accessible_name: Some(name.into()),
            label: Some(name.into()),
            description: None,
            placeholder: None,
            autocomplete: None,
            value: None,
            value_present: false,
            checked: false,
            file_count: 0,
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
            will_validate: true,
            valid: true,
            validity_flags: Vec::new(),
            validation_message: None,
            described_by: Vec::new(),
            options: Vec::new(),
            frame_path: Vec::new(),
            shadow_path: Vec::new(),
        }
    }

    #[test]
    fn normalizer_owns_public_semantics_and_never_exposes_password_text() {
        let page_id = PageId::new();
        let mut email = raw_control("email", "Email");
        email.form_key = Some("application".into());
        email.group_key = Some("contact".into());
        email.input_type = Some("email".into());
        email.value = Some("invalid".into());
        email.value_present = true;
        email.required = true;
        email.valid = false;
        email.validity_flags = vec![FormValidityFlag::TypeMismatch];
        email.validation_message = Some("Enter an email address".into());
        email.described_by = vec!["Used for notices".into()];

        let mut password = raw_control("password", "Password");
        password.form_key = Some("application".into());
        password.input_type = Some("password".into());
        password.value = Some("must-never-survive".into());
        password.value_present = true;

        let mut disabled = raw_control("disabled", "Disabled field");
        disabled.form_key = Some("application".into());
        disabled.disabled = true;

        let snapshot = normalize_form_snapshot(
            page_id.clone(),
            RawFormSnapshot {
                forms: vec![RawForm {
                    key: "application".into(),
                    accessible_name: Some("Application".into()),
                    description: Some("Apply now".into()),
                    frame_path: Vec::new(),
                    shadow_path: Vec::new(),
                }],
                groups: vec![RawFormGroup {
                    key: "contact".into(),
                    form_key: "application".into(),
                    label: Some("Contact".into()),
                    description: Some("How we reach you".into()),
                }],
                controls: vec![email, password, disabled],
                truncated: false,
            },
            512,
        )
        .unwrap();

        assert_eq!(snapshot.page_id, page_id);
        assert_eq!(snapshot.forms.len(), 1);
        let form = &snapshot.forms[0];
        assert_eq!(form.groups[0].control_ids, vec!["control-1"]);
        assert_eq!(form.validity.invalid_control_ids, vec!["control-1"]);
        assert_eq!(form.controls[0].control_kind, FormControlKind::Email);
        assert_eq!(
            form.controls[0].validity.flags,
            vec![FormValidityFlag::TypeMismatch]
        );
        assert_eq!(
            form.controls[0].supported_operations,
            vec![FormControlOperation::SetText, FormControlOperation::Clear]
        );
        assert_eq!(form.controls[1].control_kind, FormControlKind::Password);
        assert_eq!(
            form.controls[1].state,
            FormControlState::Redacted { present: true }
        );
        assert!(!serde_json::to_string(&snapshot)
            .unwrap()
            .contains("must-never-survive"));
        assert!(form.controls[2].supported_operations.is_empty());
    }

    #[test]
    fn normalizer_assigns_duplicate_ordinals_and_truncates_without_dangling_references() {
        let mut first = raw_control("home", "Phone");
        first.form_key = Some("application".into());
        let mut second = raw_control("work", "Phone");
        second.form_key = Some("application".into());
        second.shadow_path = vec![SemanticTargetSegment {
            role: "group".into(),
            accessible_name: "Work".into(),
            ordinal: None,
        }];

        let snapshot = normalize_form_snapshot(
            PageId::new(),
            RawFormSnapshot {
                forms: vec![RawForm {
                    key: "application".into(),
                    accessible_name: Some("Application".into()),
                    description: None,
                    frame_path: Vec::new(),
                    shadow_path: Vec::new(),
                }],
                groups: Vec::new(),
                controls: vec![first, second],
                truncated: false,
            },
            1,
        )
        .unwrap();

        assert!(snapshot.truncated);
        assert_eq!(snapshot.forms[0].controls.len(), 1);
        assert_eq!(
            snapshot.forms[0].controls[0]
                .target
                .as_ref()
                .unwrap()
                .ordinal,
            None
        );
        snapshot.validate().unwrap();
    }

    #[test]
    fn file_input_target_matches_the_action_resolvers_button_role() {
        let mut file = raw_control("document", "Customer document");
        file.input_type = Some("file".into());
        let snapshot = normalize_form_snapshot(
            PageId::new(),
            RawFormSnapshot {
                forms: Vec::new(),
                groups: Vec::new(),
                controls: vec![file],
                truncated: false,
            },
            512,
        )
        .unwrap();

        let control = &snapshot.unowned_controls[0];
        assert_eq!(control.control_kind, FormControlKind::File);
        assert_eq!(
            control.target.as_ref().map(|target| target.role.as_str()),
            Some("button")
        );
        assert_eq!(
            control.supported_operations,
            vec![FormControlOperation::SetFiles, FormControlOperation::Clear]
        );
    }

    #[test]
    fn control_action_compatibility_matches_snapshot_operations_and_typed_postconditions() {
        let mut checkbox = raw_control("terms", "Terms");
        checkbox.input_type = Some("checkbox".into());
        checkbox.checked = true;
        let snapshot = normalize_form_snapshot(
            PageId::new(),
            RawFormSnapshot {
                forms: Vec::new(),
                groups: Vec::new(),
                controls: vec![checkbox],
                truncated: false,
            },
            512,
        )
        .unwrap();
        let control = &snapshot.unowned_controls[0];

        let action = ControlAction::SetChecked { checked: true };
        validate_control_action(control, &action).unwrap();
        let evidence = control_action_evidence(control, &action, false, None).unwrap();
        assert_eq!(evidence.operation, FormControlOperation::SetChecked);
        assert_eq!(evidence.state, FormControlState::Checked { checked: true });
        assert!(!evidence.node_replaced);

        assert!(validate_control_action(
            control,
            &ControlAction::SetText {
                value: "wrong kind".into()
            }
        )
        .is_err());
        assert!(control_action_evidence(
            control,
            &ControlAction::SetChecked { checked: false },
            false,
            None
        )
        .is_err());
        for operation in &control.supported_operations {
            let compatible = match operation {
                FormControlOperation::SetChecked => ControlAction::SetChecked { checked: true },
                _ => panic!("unexpected advertised operation {operation:?}"),
            };
            validate_control_action(control, &compatible).unwrap();
        }
    }

    #[test]
    fn control_action_rejects_disabled_controls_and_redacts_password_receipts() {
        let mut disabled = raw_control("disabled", "Disabled");
        disabled.disabled = true;
        let mut password = raw_control("password", "Password");
        password.input_type = Some("password".into());
        password.value = None;
        password.value_present = true;
        let snapshot = normalize_form_snapshot(
            PageId::new(),
            RawFormSnapshot {
                forms: Vec::new(),
                groups: Vec::new(),
                controls: vec![disabled, password],
                truncated: false,
            },
            512,
        )
        .unwrap();

        assert!(validate_control_action(
            &snapshot.unowned_controls[0],
            &ControlAction::SetText { value: "x".into() }
        )
        .is_err());
        let evidence = control_action_evidence(
            &snapshot.unowned_controls[1],
            &ControlAction::SetText {
                value: "never-retained".into(),
            },
            false,
            None,
        )
        .unwrap();
        assert_eq!(evidence.state, FormControlState::Redacted { present: true });
        assert!(!serde_json::to_string(&evidence)
            .unwrap()
            .contains("never-retained"));
    }

    /// A SelectOne requested by label must verify against the committed
    /// option value, not the requested label string.
    #[test]
    fn select_by_label_verifies_against_the_committed_value() {
        let mut select = raw_control("priority", "Customer priority");
        select.tag = "select".into();
        select.input_type = None;
        select.options = vec![
            RawFormOption {
                value: "normal".into(),
                label: "Normal".into(),
                disabled: false,
                selected: false,
                group_label: None,
            },
            RawFormOption {
                value: "high".into(),
                label: "High".into(),
                disabled: false,
                selected: true,
                group_label: None,
            },
        ];
        let snapshot = normalize_form_snapshot(
            PageId::new(),
            RawFormSnapshot {
                forms: Vec::new(),
                groups: Vec::new(),
                controls: vec![select],
                truncated: false,
            },
            512,
        )
        .unwrap();
        let control = &snapshot.unowned_controls[0];
        let action = ControlAction::SelectOne {
            value: "High".into(),
        };
        assert!(
            control_action_evidence(control, &action, false, None).is_err(),
            "requested label must not be compared against option values"
        );
        control_action_evidence(control, &action, false, Some(&["high".into()])).unwrap();
    }

    /// An a11y-style target with explicit `ordinal: 0` must match the
    /// form-snapshot target that omits it for a unique control.
    #[test]
    fn explicit_zero_ordinal_matches_an_omitted_ordinal() {
        let base = types::FormControlTarget {
            role: "combobox".into(),
            accessible_name: "Customer priority".into(),
            ordinal: None,
            frame_path: Vec::new(),
            shadow_path: Vec::new(),
        };
        let mut explicit = base.clone();
        explicit.ordinal = Some(0);
        assert!(target_specs_equivalent(&base, &explicit));
        let mut second = base.clone();
        second.ordinal = Some(1);
        assert!(!target_specs_equivalent(&base, &second));
        let mut cased = base.clone();
        cased.role = "ComboBox".into();
        assert!(target_specs_equivalent(&base, &cased));
    }
}
