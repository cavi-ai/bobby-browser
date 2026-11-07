//! Workflow files: named step lists that `workflow_run` runs by name. A file
//! is data: each step calls one MCP tool or runs another workflow, and values
//! pass between steps only through `$input`, `$step` and `$item` refs.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Map, Value};

/// The JSON Schema every workflow file must satisfy.
pub const WORKFLOW_SCHEMA: &str = include_str!("workflow.schema.json");

pub(crate) const WORKFLOWS_URI: &str = "bobby://workflows";
pub(crate) const WORKFLOW_SCHEMA_URI: &str = "bobby://workflow-schema";

/// Workflows the gateway runs natively; file names cannot take these.
pub(crate) const BUILTIN_WORKFLOWS: &[&str] = &["fillForm", "extractPage", "collectLinks"];

/// Tools a step may not call: they run arbitrary code, change cookies, or
/// leave the run's session, page and workflow.
pub(crate) const EXCLUDED_TOOLS: &[&str] = &[
    "click_and_wait_for_popup",
    "command_execute",
    "cookie_delete",
    "cookie_set",
    "evaluate_javascript",
    "job_cancel",
    "job_submit",
    "page_activate",
    "page_close",
    "page_open",
    "session_close",
    "session_create",
    "toolset_select",
    "workflow_recover",
    "workflow_run",
    "workflow_start",
];

/// Arguments the run supplies to every tool step.
const SCOPE_ARGUMENTS: &[&str] = &["sessionId", "pageId", "workflowId", "workflowHandle"];

/// Most nested workflow levels below the one called.
pub(crate) const MAX_NESTING: usize = 2;
/// Most items one `forEach` step runs.
pub(crate) const MAX_FOR_EACH_ITEMS: usize = 50;
/// Most tool calls one run makes, nested workflows included.
pub(crate) const MAX_RUN_CALLS: usize = 256;
/// Largest workflow file read.
const MAX_FILE_BYTES: u64 = 256 * 1024;
/// Most files a directory listing reads.
const MAX_LISTED_FILES: usize = 256;

/// Why a workflow file cannot run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowError {
    /// Stable code, such as `unknownTool` or `schemaViolation`.
    pub code: &'static str,
    pub message: String,
    /// JSON Pointer into the file where the problem is.
    pub pointer: String,
}

impl WorkflowError {
    fn new(code: &'static str, pointer: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            pointer: pointer.into(),
        }
    }

    pub(crate) fn to_value(&self) -> Value {
        json!({"code":self.code,"message":self.message,"pointer":self.pointer})
    }
}

impl fmt::Display for WorkflowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pointer = if self.pointer.is_empty() {
            "/"
        } else {
            &self.pointer
        };
        write!(formatter, "{} at {pointer}: {}", self.code, self.message)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Definition {
    #[serde(rename = "$schema", default)]
    _schema: Option<String>,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) inputs: BTreeMap<String, InputSpec>,
    pub(crate) steps: Vec<Step>,
    #[serde(default)]
    pub(crate) outputs: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InputSpec {
    #[serde(rename = "type")]
    pub(crate) kind: InputType,
    #[serde(default)]
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) default: Option<Value>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum InputType {
    String,
    Number,
    Boolean,
    Array,
    Object,
}

impl InputType {
    fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Array => "array",
            Self::Object => "object",
        }
    }

    fn admits(self, value: &Value) -> bool {
        match self {
            Self::String => value.is_string(),
            Self::Number => value.is_number(),
            Self::Boolean => value.is_boolean(),
            Self::Array => value.is_array(),
            Self::Object => value.is_object(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct Step {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) tool: Option<String>,
    #[serde(default)]
    pub(crate) workflow: Option<String>,
    #[serde(default)]
    pub(crate) args: Map<String, Value>,
    #[serde(default)]
    pub(crate) when: Option<Value>,
    #[serde(default)]
    pub(crate) for_each: Option<Value>,
    #[serde(default)]
    pub(crate) continue_on: Vec<String>,
}

/// Whether `name` can be a workflow file's name.
pub(crate) fn valid_file_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Parses and checks a workflow file whose file name is `<file_name>.json`.
pub(crate) fn parse(file_name: &str, bytes: &[u8]) -> Result<Definition, WorkflowError> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| WorkflowError::new("invalidJson", "", error.to_string()))?;
    let mut schema: Value = serde_json::from_str(WORKFLOW_SCHEMA).expect("workflow schema is JSON");
    // The gateway's validator evaluates no regex; `check` enforces the
    // schema's name patterns.
    strip_patterns(&mut schema);
    crate::schema::validate_document(&schema, &value).map_err(|violation| {
        WorkflowError::new(
            "schemaViolation",
            violation.pointer.clone(),
            format!("violates `{}`", violation.constraint),
        )
    })?;
    let definition: Definition = serde_json::from_value(value)
        .map_err(|error| WorkflowError::new("schemaViolation", "", error.to_string()))?;
    check(file_name, &definition)?;
    Ok(definition)
}

fn strip_patterns(schema: &mut Value) {
    match schema {
        Value::Object(object) => {
            object.remove("pattern");
            object.values_mut().for_each(strip_patterns);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_patterns),
        _ => {}
    }
}

/// A letter of `first`'s kind, then up to 31 ASCII letters and digits.
fn valid_identifier(name: &str, first: fn(&char) -> bool) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| first(&c))
        && name.len() <= 32
        && chars.all(|c| c.is_ascii_alphanumeric())
}

fn check(file_name: &str, definition: &Definition) -> Result<(), WorkflowError> {
    if !valid_file_name(&definition.name) {
        return Err(WorkflowError::new(
            "schemaViolation",
            "/name",
            "a name is lowercase letters, digits and hyphens",
        ));
    }
    if let Some(name) = definition
        .inputs
        .keys()
        .find(|name| !valid_identifier(name, char::is_ascii_alphabetic))
    {
        return Err(WorkflowError::new(
            "schemaViolation",
            format!("/inputs/{}", escape(name)),
            "an input name is a letter, then letters and digits",
        ));
    }
    if let Some(index) = definition
        .steps
        .iter()
        .position(|step| !valid_identifier(&step.id, char::is_ascii_lowercase))
    {
        return Err(WorkflowError::new(
            "schemaViolation",
            format!("/steps/{index}/id"),
            "a step id is a lowercase letter, then letters and digits",
        ));
    }
    if definition.name != file_name {
        return Err(WorkflowError::new(
            "nameMismatch",
            "/name",
            format!("name must equal the file name `{file_name}`"),
        ));
    }
    for (name, input) in &definition.inputs {
        if let Some(default) = &input.default {
            if !input.kind.admits(default) {
                return Err(WorkflowError::new(
                    "defaultType",
                    format!("/inputs/{name}/default"),
                    format!("default is not a {}", input.kind.as_str()),
                ));
            }
        }
    }
    let mut earlier: Vec<&str> = Vec::new();
    for (index, step) in definition.steps.iter().enumerate() {
        let at = format!("/steps/{index}");
        if earlier.contains(&step.id.as_str()) {
            return Err(WorkflowError::new(
                "duplicateStepId",
                format!("{at}/id"),
                format!("step id `{}` is used twice", step.id),
            ));
        }
        if let Some(tool) = &step.tool {
            if crate::catalog::descriptor(tool).is_none() {
                return Err(WorkflowError::new(
                    "unknownTool",
                    format!("{at}/tool"),
                    format!("`{tool}` is not an MCP tool"),
                ));
            }
            if EXCLUDED_TOOLS.contains(&tool.as_str()) {
                return Err(WorkflowError::new(
                    "excludedTool",
                    format!("{at}/tool"),
                    format!("`{tool}` cannot run in a workflow"),
                ));
            }
            if let Some(scoped) = SCOPE_ARGUMENTS
                .iter()
                .find(|key| step.args.contains_key(**key))
            {
                return Err(WorkflowError::new(
                    "scopedArgument",
                    format!("{at}/args/{scoped}"),
                    "the run supplies sessionId, pageId and workflowId",
                ));
            }
        }
        if let Some(workflow) = &step.workflow {
            if !BUILTIN_WORKFLOWS.contains(&workflow.as_str()) && !valid_file_name(workflow) {
                return Err(WorkflowError::new(
                    "unknownWorkflow",
                    format!("{at}/workflow"),
                    format!("`{workflow}` is neither a built-in nor a workflow file name"),
                ));
            }
        }
        let scope = RefScope {
            inputs: &definition.inputs,
            steps: &earlier,
            item: false,
        };
        if let Some(when) = &step.when {
            check_refs(when, &format!("{at}/when"), &scope)?;
        }
        if let Some(for_each) = &step.for_each {
            check_refs(for_each, &format!("{at}/forEach"), &scope)?;
            let array = for_each["$input"]
                .as_str()
                .and_then(|name| definition.inputs.get(name))
                .is_some_and(|input| input.kind == InputType::Array);
            if !array {
                return Err(WorkflowError::new(
                    "forEachNotArray",
                    format!("{at}/forEach"),
                    "forEach must name an input of type array",
                ));
            }
        }
        let scope = RefScope {
            item: step.for_each.is_some(),
            ..scope
        };
        check_refs(
            &Value::Object(step.args.clone()),
            &format!("{at}/args"),
            &scope,
        )?;
        earlier.push(&step.id);
    }
    let scope = RefScope {
        inputs: &definition.inputs,
        steps: &earlier,
        item: false,
    };
    for (name, output) in &definition.outputs {
        check_refs(output, &format!("/outputs/{name}"), &scope)?;
    }
    Ok(())
}

struct RefScope<'a> {
    inputs: &'a BTreeMap<String, InputSpec>,
    /// Step ids a `$step` ref may name.
    steps: &'a [&'a str],
    /// Whether `$item` refs are allowed.
    item: bool,
}

fn check_refs(value: &Value, at: &str, scope: &RefScope<'_>) -> Result<(), WorkflowError> {
    match value {
        Value::Object(object) if is_ref(object) => check_ref(object, at, scope),
        Value::Object(object) => object.iter().try_for_each(|(key, value)| {
            check_refs(value, &format!("{at}/{}", escape(key)), scope)
        }),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .try_for_each(|(index, value)| check_refs(value, &format!("{at}/{index}"), scope)),
        _ => Ok(()),
    }
}

fn is_ref(object: &Map<String, Value>) -> bool {
    ["$input", "$step", "$item"]
        .iter()
        .any(|key| object.contains_key(*key))
}

fn check_ref(
    object: &Map<String, Value>,
    at: &str,
    scope: &RefScope<'_>,
) -> Result<(), WorkflowError> {
    let keys: BTreeSet<&str> = object.keys().map(String::as_str).collect();
    let bad = |message: &str| Err(WorkflowError::new("badRef", at, message));
    if let Some(name) = object.get("$input") {
        if keys.len() != 1 {
            return bad("an $input ref has no other keys");
        }
        let Some(name) = name.as_str() else {
            return bad("$input names an input");
        };
        if !scope.inputs.contains_key(name) {
            return Err(WorkflowError::new(
                "unknownInput",
                at,
                format!("input `{name}` is not declared"),
            ));
        }
        return Ok(());
    }
    if let Some(step) = object.get("$step") {
        if !keys
            .iter()
            .all(|key| ["$step", "pointer", "field"].contains(key))
            || (keys.contains("pointer") && keys.contains("field"))
        {
            return bad("a $step ref takes pointer or field, not both");
        }
        let Some(step) = step.as_str() else {
            return bad("$step names a step");
        };
        if !scope.steps.contains(&step) {
            return Err(WorkflowError::new(
                "unknownStep",
                at,
                format!("`{step}` is not an earlier step"),
            ));
        }
        if let Some(pointer) = object.get("pointer") {
            if !pointer
                .as_str()
                .is_some_and(|pointer| pointer.is_empty() || pointer.starts_with('/'))
            {
                return bad("pointer is a JSON Pointer");
            }
        }
        if object.get("field").is_some_and(|field| !field.is_string()) {
            return bad("field names an extraction field");
        }
        return Ok(());
    }
    if keys.len() != 1
        || !object["$item"]
            .as_str()
            .is_some_and(|pointer| pointer.is_empty() || pointer.starts_with('/'))
    {
        return bad("$item is a JSON Pointer into the item, alone");
    }
    if !scope.item {
        return Err(WorkflowError::new(
            "itemOutsideForEach",
            at,
            "$item is only valid in the args of a forEach step",
        ));
    }
    Ok(())
}

fn escape(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

/// Why a named workflow file did not load.
pub(crate) enum LoadFailure {
    NotFound,
    Invalid(PathBuf, WorkflowError),
}

/// The first `<dir>/<name>.json` in `dirs`, parsed and checked.
pub(crate) fn load(dirs: &[PathBuf], name: &str) -> Result<(PathBuf, Definition), LoadFailure> {
    if !valid_file_name(name) {
        return Err(LoadFailure::NotFound);
    }
    let path = dirs
        .iter()
        .map(|dir| dir.join(format!("{name}.json")))
        .find(|path| path.is_file())
        .ok_or(LoadFailure::NotFound)?;
    match read(&path).and_then(|bytes| parse(name, &bytes)) {
        Ok(definition) => Ok((path, definition)),
        Err(error) => Err(LoadFailure::Invalid(path, error)),
    }
}

fn read(path: &Path) -> Result<Vec<u8>, WorkflowError> {
    let unreadable =
        |error: std::io::Error| WorkflowError::new("unreadable", "", error.to_string());
    let size = std::fs::metadata(path).map_err(unreadable)?.len();
    if size > MAX_FILE_BYTES {
        return Err(WorkflowError::new(
            "fileTooLarge",
            "",
            format!("over {MAX_FILE_BYTES} bytes"),
        ));
    }
    std::fs::read(path).map_err(unreadable)
}

/// Checks the workflow file at `path`, named by its file name.
pub fn validate_file(path: &Path) -> Result<(), WorkflowError> {
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|_| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .ok_or_else(|| WorkflowError::new("nameMismatch", "", "a workflow file is <name>.json"))?;
    if !valid_file_name(name) {
        return Err(WorkflowError::new(
            "nameMismatch",
            "",
            "a workflow file name is lowercase letters, digits and hyphens",
        ));
    }
    read(path).and_then(|bytes| parse(name, &bytes)).map(|_| ())
}

/// A `.json` file found in a workflow directory.
#[derive(Debug, Clone)]
pub struct ListedWorkflow {
    pub name: String,
    pub path: PathBuf,
    pub description: Option<String>,
    pub inputs: Value,
    pub error: Option<WorkflowError>,
}

/// Every `<name>.json` in `dirs`, the first of each name, sorted by name.
pub fn list_files(dirs: &[PathBuf]) -> Vec<ListedWorkflow> {
    let mut listed: BTreeMap<String, ListedWorkflow> = BTreeMap::new();
    let mut read_files = 0;
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect();
        paths.sort();
        for path in paths {
            let Some(name) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            if listed.contains_key(&name) || read_files == MAX_LISTED_FILES {
                continue;
            }
            read_files += 1;
            let entry = match validate_and_describe(&path, &name) {
                Ok(definition) => ListedWorkflow {
                    name: name.clone(),
                    path,
                    description: definition.description.clone(),
                    inputs: inputs_listing(&definition.inputs),
                    error: None,
                },
                Err(error) => ListedWorkflow {
                    name: name.clone(),
                    path,
                    description: None,
                    inputs: json!({}),
                    error: Some(error),
                },
            };
            listed.insert(name, entry);
        }
    }
    listed.into_values().collect()
}

fn validate_and_describe(path: &Path, name: &str) -> Result<Definition, WorkflowError> {
    if !valid_file_name(name) {
        return Err(WorkflowError::new(
            "nameMismatch",
            "",
            "a workflow file name is lowercase letters, digits and hyphens",
        ));
    }
    read(path).and_then(|bytes| parse(name, &bytes))
}

fn inputs_listing(inputs: &BTreeMap<String, InputSpec>) -> Value {
    inputs
        .iter()
        .map(|(name, input)| {
            let mut entry = json!({"type":input.kind.as_str(),"required":input.default.is_none()});
            if let Some(description) = &input.description {
                entry["description"] = json!(description);
            }
            if let Some(default) = &input.default {
                entry["default"] = default.clone();
            }
            (name.clone(), entry)
        })
        .collect::<Map<String, Value>>()
        .into()
}

/// The built-in workflows, listed like workflow files.
pub(crate) fn builtin_listing() -> Vec<Value> {
    let url = json!({"type":"string","required":false,"description":"Opened first."});
    vec![
        json!({"name":"fillForm","builtin":true,
               "description":"intent_complete_form, then intent_submit_and_verify when submit is set.",
               "inputs":{"url":url,
                         "fields":{"type":"array","required":true,"description":"intent_complete_form fields."},
                         "submit":{"type":"object","required":false,"description":"intent_submit_and_verify arguments."},
                         "screenshot":{"type":"boolean","required":false,"description":"A screenshot on success."}}}),
        json!({"name":"extractPage","builtin":true,
               "description":"intent_extract of the named fields.",
               "inputs":{"url":url,
                         "fields":{"type":"array","required":true,"description":"intent_extract fields."}}}),
        json!({"name":"collectLinks","builtin":true,
               "description":"The links in list, following next up to maxPages pages.",
               "inputs":{"url":url,
                         "list":{"type":"object","required":true,"description":"Target of the link list."},
                         "next":{"type":"object","required":false,"description":"intent_follow arguments for the next page."},
                         "maxPages":{"type":"number","required":false,"default":1}}}),
    ]
}

/// `bobby://workflows`: the built-ins, then every workflow file in `dirs`.
pub(crate) fn listing(dirs: &[PathBuf]) -> Value {
    let files = list_files(dirs).into_iter().map(|listed| {
        let mut entry = json!({"name":listed.name,"path":listed.path.display().to_string(),
                               "inputs":listed.inputs});
        if let Some(description) = listed.description {
            entry["description"] = json!(description);
        }
        if let Some(error) = listed.error {
            entry["error"] = error.to_value();
        }
        entry
    });
    json!({"workflows":builtin_listing().into_iter().chain(files).collect::<Vec<_>>()})
}

/// The text of a `bobby://workflow-schema`, `bobby://workflows` or
/// `bobby://workflows/<name>` resource.
pub(crate) fn resource_text(dirs: &[PathBuf], uri: &str) -> Option<String> {
    if uri == WORKFLOW_SCHEMA_URI {
        return Some(WORKFLOW_SCHEMA.to_owned());
    }
    if uri == WORKFLOWS_URI {
        return Some(listing(dirs).to_string());
    }
    let name = uri.strip_prefix(WORKFLOWS_URI)?.strip_prefix('/')?;
    if let Some(builtin) = builtin_listing()
        .into_iter()
        .find(|entry| entry["name"] == name)
    {
        return Some(builtin.to_string());
    }
    match load(dirs, name) {
        Ok((path, _)) => read(&path)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok()),
        Err(LoadFailure::Invalid(path, error)) => Some(
            json!({"name":name,"path":path.display().to_string(),"error":error.to_value()})
                .to_string(),
        ),
        Err(LoadFailure::NotFound) => None,
    }
}

/// The call's inputs with defaults applied, or why they do not fit.
pub(crate) fn resolve_inputs(
    definition: &Definition,
    given: &Map<String, Value>,
) -> Result<Map<String, Value>, String> {
    if let Some(unknown) = given
        .keys()
        .find(|name| !definition.inputs.contains_key(*name))
    {
        return Err(format!(
            "`{unknown}` is not an input of `{}`",
            definition.name
        ));
    }
    let mut resolved = Map::new();
    for (name, input) in &definition.inputs {
        let value = match (given.get(name), &input.default) {
            (Some(value), _) => value.clone(),
            (None, Some(default)) => default.clone(),
            (None, None) => return Err(format!("input `{name}` is required")),
        };
        if !input.kind.admits(&value) {
            return Err(format!("input `{name}` is not a {}", input.kind.as_str()));
        }
        resolved.insert(name.clone(), value);
    }
    Ok(resolved)
}

/// Replaces every ref in `value` with the value it names; a ref to nothing
/// reads as null.
pub(crate) fn resolve(
    value: &Value,
    inputs: &Map<String, Value>,
    results: &BTreeMap<String, Value>,
    item: Option<&Value>,
) -> Value {
    match value {
        Value::Object(object) if is_ref(object) => {
            if let Some(name) = object.get("$input").and_then(Value::as_str) {
                return inputs.get(name).cloned().unwrap_or(Value::Null);
            }
            if let Some(step) = object.get("$step").and_then(Value::as_str) {
                let Some(result) = results.get(step) else {
                    return Value::Null;
                };
                if let Some(field) = object.get("field").and_then(Value::as_str) {
                    return extracted(result, field);
                }
                let pointer = object.get("pointer").and_then(Value::as_str).unwrap_or("");
                return result.pointer(pointer).cloned().unwrap_or(Value::Null);
            }
            let pointer = object.get("$item").and_then(Value::as_str).unwrap_or("");
            item.and_then(|item| item.pointer(pointer))
                .cloned()
                .unwrap_or(Value::Null)
        }
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), resolve(value, inputs, results, item)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|value| resolve(value, inputs, results, item))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The value a step's extraction evidence holds for `field`.
fn extracted(result: &Value, field: &str) -> Value {
    result["evidence"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|item| item["kind"] == "extraction" && item["field"] == field)
        .map(|item| item["value"].clone())
        .unwrap_or(Value::Null)
}

/// Whether a `when` value lets its step run.
pub(crate) fn truthy(value: &Value) -> bool {
    !matches!(value, Value::Null | Value::Bool(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(definition: Value) -> WorkflowError {
        let bytes = serde_json::to_vec(&definition).unwrap();
        parse("probe", &bytes).expect_err("the definition is rejected")
    }

    fn navigate(id: &str, url: Value) -> Value {
        json!({"id":id,"tool":"navigate","args":{"url":url}})
    }

    #[test]
    fn the_schema_accepts_its_own_example_shape() {
        let definition = json!({"name":"probe","inputs":{"url":{"type":"string"},"urls":{"type":"array","default":[]}},
            "steps":[navigate("open", json!({"$input":"url"})),
                     {"id":"each","workflow":"probe-item","forEach":{"$input":"urls"},
                      "args":{"url":{"$item":""}},"continueOn":["targetNotFound"]}],
            "outputs":{"first":{"$step":"open","pointer":"/status"}}});
        parse("probe", &serde_json::to_vec(&definition).unwrap()).expect("valid definition");
    }

    #[test]
    fn each_load_error_names_its_code_and_place() {
        let cases = [
            (
                json!({"name":"other","steps":[navigate("open", json!("x"))]}),
                "nameMismatch",
                "/name",
            ),
            (
                json!({"name":"probe","steps":[]}),
                "schemaViolation",
                "/steps",
            ),
            (
                json!({"name":"probe","steps":[{"id":"x","tool":"no_such_tool"}]}),
                "unknownTool",
                "/steps/0/tool",
            ),
            (
                json!({"name":"probe","steps":[{"id":"x","tool":"evaluate_javascript"}]}),
                "excludedTool",
                "/steps/0/tool",
            ),
            (
                json!({"name":"probe","steps":[{"id":"x","tool":"navigate","args":{"url":"u","sessionId":"s"}}]}),
                "scopedArgument",
                "/steps/0/args/sessionId",
            ),
            (
                json!({"name":"probe","steps":[navigate("x", json!({"$input":"missing"}))]}),
                "unknownInput",
                "/steps/0/args/url",
            ),
            (
                json!({"name":"probe","steps":[navigate("x", json!({"$step":"later"})), navigate("later", json!("u"))]}),
                "unknownStep",
                "/steps/0/args/url",
            ),
            (
                json!({"name":"probe","steps":[navigate("x", json!({"$item":""}))]}),
                "itemOutsideForEach",
                "/steps/0/args/url",
            ),
            (
                json!({"name":"probe","steps":[navigate("x", json!("u")), navigate("x", json!("v"))]}),
                "duplicateStepId",
                "/steps/1/id",
            ),
            (
                json!({"name":"probe","inputs":{"n":{"type":"string","default":1}},"steps":[navigate("x", json!("u"))]}),
                "defaultType",
                "/inputs/n/default",
            ),
            (
                json!({"name":"probe","inputs":{"n":{"type":"string"}},
                    "steps":[{"id":"x","tool":"navigate","forEach":{"$input":"n"},"args":{"url":"u"}}]}),
                "forEachNotArray",
                "/steps/0/forEach",
            ),
            (
                json!({"name":"probe","steps":[{"id":"x","workflow":"Not_A_Name"}]}),
                "unknownWorkflow",
                "/steps/0/workflow",
            ),
            (
                json!({"name":"probe","steps":[navigate("x", json!("u"))],"outputs":{"o":{"$step":"x","pointer":"a"}}}),
                "badRef",
                "/outputs/o",
            ),
        ];
        for (definition, code, pointer) in cases {
            let error = error(definition);
            assert_eq!(
                (error.code, error.pointer.as_str()),
                (code, pointer),
                "{error}"
            );
        }
    }

    #[test]
    fn refs_resolve_inputs_results_fields_and_items() {
        let inputs = Map::from_iter([("a".to_owned(), json!("x"))]);
        let results = BTreeMap::from([(
            "read".to_owned(),
            json!({"status":"completed","evidence":[{"kind":"extraction","field":"f","value":"v"}]}),
        )]);
        let item = json!({"u":"i"});
        let resolved = resolve(
            &json!({"a":{"$input":"a"},"s":{"$step":"read","pointer":"/status"},
                    "f":{"$step":"read","field":"f"},"i":{"$item":"/u"},"n":{"$step":"gone"},
                    "list":[{"$input":"a"}]}),
            &inputs,
            &results,
            Some(&item),
        );
        assert_eq!(
            resolved,
            json!({"a":"x","s":"completed","f":"v","i":"i","n":null,"list":["x"]})
        );
    }
}
