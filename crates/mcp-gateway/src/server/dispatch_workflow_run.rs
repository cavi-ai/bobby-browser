//! `workflow_run`: a built-in or file workflow that runs existing tools in a
//! fixed order inside one call. Each tool step goes through `call_tool`, so it
//! keeps that tool's authorization, validation, guards and evidence.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Map;

use super::*;
use crate::schema::{MAX_WORKFLOW_LINKS, MAX_WORKFLOW_PAGES};
use crate::tool_args::{BuiltinInputs, NamedWorkflow, WorkflowRunArgs};
use crate::workflows::{self, Definition, LoadFailure};

/// File-workflow runs a connection keeps for `runId` reruns.
const MAX_STORED_RUNS: usize = 16;

/// The settle wait `collectLinks` uses after opening the next page when
/// `next` names none.
fn next_page_settled() -> Value {
    json!({"condition":{"kind":"networkQuiet","idleMs":200,"maxInFlight":0},"timeoutMs":10000})
}

fn failed(code: &str, message: impl Into<String>) -> Value {
    json!({"status":"failed","error":{"code":code,"message":message.into()}})
}

/// The error code of a step result that did not complete, if it did not.
fn failure_code(result: &Value) -> Option<&str> {
    match result["status"].as_str() {
        Some("completed" | "skipped") => None,
        _ => Some(result["error"]["code"].as_str().unwrap_or("failed")),
    }
}

/// A file-workflow run kept for a rerun with `runId` and `from`.
pub(super) struct StoredRun {
    run_id: String,
    workflow: String,
    /// One `{id, tool|workflow, result}` entry per step that ran or was reused.
    entries: Vec<Value>,
    /// Index of the step the run stopped at.
    stopped_at: Option<usize>,
}

struct Run {
    workflow: String,
    workflow_id: types::WorkflowId,
    steps: Vec<Value>,
    screenshots: Vec<Value>,
    /// The step name, status and error the run stopped at.
    failure: Option<(String, Value, Value)>,
}

impl Run {
    fn new(workflow: &str, workflow_id: types::WorkflowId) -> Self {
        Self {
            workflow: workflow.to_owned(),
            workflow_id,
            steps: Vec::new(),
            screenshots: Vec::new(),
            failure: None,
        }
    }

    fn failed(&self) -> bool {
        self.failure.is_some()
    }

    /// The outcome a step's response carries; a refused call reads as a
    /// failed outcome with the refusal's message.
    fn outcome(response: &Value) -> Value {
        if let Some(result) = response.pointer("/result/structuredContent") {
            return result.clone();
        }
        let refusal = response.get("error").cloned().unwrap_or(Value::Null);
        json!({
            "status":"failed",
            "error":{
                "code":"invalidRequest",
                "message":refusal.get("message").cloned().unwrap_or(json!("step refused")),
                "data":refusal.get("data").cloned().unwrap_or(Value::Null)
            }
        })
    }

    fn hoist_screenshots(&mut self, result: &Value) {
        match result {
            Value::Array(items) => items.iter().for_each(|item| self.hoist_screenshots(item)),
            result => {
                if let Some(evidence) = result["evidence"].as_array() {
                    self.screenshots.extend(
                        evidence
                            .iter()
                            .filter(|item| item["kind"] == "screenshot")
                            .cloned(),
                    );
                }
            }
        }
    }

    /// Records a built-in step, stopping the run when it did not complete.
    fn record(&mut self, tool: &str, result: Value) {
        if failure_code(&result).is_some() {
            self.failure = Some((
                tool.to_owned(),
                result["status"].clone(),
                result["error"].clone(),
            ));
        }
        self.hoist_screenshots(&result);
        self.steps.push(json!({"tool":tool,"result":result}));
    }

    fn into_value(self) -> Value {
        let mut value = json!({
            "status":"completed",
            "workflow":self.workflow,
            "workflowId":self.workflow_id,
            "steps":self.steps,
            "evidence":self.screenshots
        });
        if let Some((step, status, error)) = self.failure {
            value["status"] = status;
            value["stoppedAt"] = json!(step);
            if !error.is_null() {
                value["error"] = error;
            }
        }
        value
    }
}

/// Every `{name, url}` pair on a link node, in document order.
fn links_in(value: &Value, out: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) => {
            if map.get("role").and_then(Value::as_str) == Some("link") {
                if let Some(url) = map.get("url").and_then(Value::as_str) {
                    let name = map
                        .get("name")
                        .or_else(|| map.get("target")?.get("accessibleName"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    out.push((name.to_owned(), url.to_owned()));
                }
            }
            map.values().for_each(|child| links_in(child, out));
        }
        Value::Array(items) => items.iter().for_each(|child| links_in(child, out)),
        _ => {}
    }
}

/// A built-in workflow's inputs, or why they do not fit it.
fn builtin_inputs(
    workflow: NamedWorkflow,
    inputs: &Map<String, Value>,
) -> Result<BuiltinInputs, String> {
    let inputs: BuiltinInputs = serde_json::from_value(Value::Object(inputs.clone()))
        .map_err(|error| format!("{} inputs: {error}", workflow.as_str()))?;
    let allowed: &[&str] = match workflow {
        NamedWorkflow::FillForm => &["url", "fields", "submit", "screenshot"],
        NamedWorkflow::ExtractPage => &["url", "fields"],
        NamedWorkflow::CollectLinks => &["url", "list", "next", "maxPages"],
    };
    let given = [
        ("url", inputs.url.is_some()),
        ("fields", inputs.fields.is_some()),
        ("submit", inputs.submit.is_some()),
        ("screenshot", inputs.screenshot.is_some()),
        ("list", inputs.list.is_some()),
        ("next", inputs.next.is_some()),
        ("maxPages", inputs.max_pages.is_some()),
    ];
    if let Some((name, _)) = given
        .iter()
        .find(|(name, set)| *set && !allowed.contains(name))
    {
        return Err(format!("`{name}` is not an input of {}", workflow.as_str()));
    }
    let required = match workflow {
        NamedWorkflow::FillForm | NamedWorkflow::ExtractPage => ("fields", inputs.fields.is_some()),
        NamedWorkflow::CollectLinks => ("list", inputs.list.is_some()),
    };
    if !required.1 {
        return Err(format!("input `{}` is required", required.0));
    }
    if inputs
        .max_pages
        .is_some_and(|pages| !(1..=MAX_WORKFLOW_PAGES).contains(&pages))
    {
        return Err(format!("maxPages is 1 to {MAX_WORKFLOW_PAGES}"));
    }
    Ok(inputs)
}

/// What a file-workflow run needs besides its definition.
struct Frame<'a> {
    id: &'a Value,
    scope: &'a Value,
    workflow_id: &'a types::WorkflowId,
    depth: usize,
    calls: &'a AtomicUsize,
}

type Boxed<'a> = Pin<Box<dyn Future<Output = Value> + Send + 'a>>;

/// A file run's value, its step entries, and the index of the step it stopped at.
type FileRun = (Value, Vec<Value>, Option<usize>);

impl Server {
    /// One tool call with the run's scope added where the tool takes it.
    async fn run_tool(&self, frame: &Frame<'_>, tool: &str, arguments: Value) -> Value {
        if frame.calls.fetch_add(1, Ordering::SeqCst) >= workflows::MAX_RUN_CALLS {
            return failed(
                "workflowTooLong",
                format!(
                    "a run makes at most {} tool calls",
                    workflows::MAX_RUN_CALLS
                ),
            );
        }
        let declared = crate::catalog::descriptor(tool).map(|descriptor| (descriptor.schema)().0);
        let mut merged = json!({});
        for key in ["sessionId", "pageId", "workflowId"] {
            if declared
                .as_ref()
                .is_some_and(|properties| properties.get(key).is_some())
            {
                merged[key] = frame.scope[key].clone();
            }
        }
        for (key, value) in arguments.as_object().into_iter().flatten() {
            merged[key] = value.clone();
        }
        let params = json!({"name":tool,"arguments":merged});
        Run::outcome(&self.call_step(frame.id.clone(), params).await)
    }

    /// A named return type keeps the recursion through `call_tool` from
    /// making its future's `Send` depend on itself.
    fn call_step(&self, id: Value, params: Value) -> Boxed<'_> {
        Box::pin(self.call_tool(id, params))
    }

    async fn builtin_step(
        &self,
        frame: &Frame<'_>,
        run: &mut Run,
        tool: &str,
        arguments: Value,
    ) -> bool {
        let result = self.run_tool(frame, tool, arguments).await;
        run.record(tool, result);
        !run.failed()
    }

    fn run_builtin<'a>(
        &'a self,
        frame: &'a Frame<'a>,
        workflow: NamedWorkflow,
        input: BuiltinInputs,
    ) -> Boxed<'a> {
        Box::pin(async move {
            let mut run = Run::new(workflow.as_str(), frame.workflow_id.clone());
            let opened = match &input.url {
                Some(url) => {
                    self.builtin_step(frame, &mut run, "navigate", json!({"url":url}))
                        .await
                }
                None => true,
            };
            let mut collected = None;
            match workflow {
                NamedWorkflow::FillForm => {
                    let mut fill = json!({"purpose":"fill the form","fields":input.fields});
                    if let Some(screenshot) = input.screenshot {
                        fill["screenshot"] = json!(screenshot);
                    }
                    if opened
                        && self
                            .builtin_step(frame, &mut run, "intent_complete_form", fill)
                            .await
                    {
                        if let Some(mut submit) = input.submit {
                            if let Some(screenshot) = input.screenshot {
                                submit.insert("screenshot".to_owned(), json!(screenshot));
                            }
                            self.builtin_step(
                                frame,
                                &mut run,
                                "intent_submit_and_verify",
                                Value::Object(submit),
                            )
                            .await;
                        }
                    }
                }
                NamedWorkflow::ExtractPage => {
                    if opened {
                        let extract = json!({"purpose":"read the page","fields":input.fields});
                        self.builtin_step(frame, &mut run, "intent_extract", extract)
                            .await;
                    }
                }
                NamedWorkflow::CollectLinks => {
                    let list = Value::Object(input.list.unwrap_or_default());
                    let max_pages = input.max_pages.unwrap_or(1);
                    let mut links: Vec<(String, String)> = Vec::new();
                    let mut pages = 0;
                    while opened && pages < max_pages {
                        let snapshot = self
                            .run_tool(frame, "a11y_snapshot", json!({"target":list}))
                            .await;
                        if snapshot["status"] != "completed" {
                            run.record("a11y_snapshot", snapshot);
                            break;
                        }
                        let mut found = Vec::new();
                        links_in(&snapshot, &mut found);
                        run.record(
                            "a11y_snapshot",
                            json!({"status":"completed","links":found.len()}),
                        );
                        for link in found {
                            if links.len() < MAX_WORKFLOW_LINKS
                                && !links.iter().any(|known| known.1 == link.1)
                            {
                                links.push(link);
                            }
                        }
                        pages += 1;
                        let Some(next) = input.next.as_ref().filter(|_| pages < max_pages) else {
                            break;
                        };
                        let mut follow = next.clone();
                        follow
                            .entry("expectedState")
                            .or_insert_with(next_page_settled);
                        let followed = self
                            .run_tool(frame, "intent_follow", Value::Object(follow))
                            .await;
                        // No next control: the list ended.
                        if followed["error"]["code"] == "targetNotFound" {
                            run.steps
                                .push(json!({"tool":"intent_follow","result":followed}));
                            break;
                        }
                        run.record("intent_follow", followed);
                        if run.failed() {
                            break;
                        }
                    }
                    let links: Vec<Value> = links
                        .into_iter()
                        .map(|(name, url)| json!({"name":name,"url":url}))
                        .collect();
                    collected = Some((links, pages));
                }
            }
            let mut value = run.into_value();
            if let Some((links, pages)) = collected {
                value["links"] = json!(links);
                value["pages"] = json!(pages);
            }
            value
        })
    }

    /// One invocation of a step: its tool, or the workflow it names.
    async fn run_unit(&self, frame: &Frame<'_>, step: &workflows::Step, args: Value) -> Value {
        if let Some(tool) = &step.tool {
            return self.run_tool(frame, tool, args).await;
        }
        let name = step.workflow.as_deref().unwrap_or_default();
        if frame.depth >= workflows::MAX_NESTING {
            return failed(
                "workflowTooDeep",
                format!("workflows nest at most {} levels", workflows::MAX_NESTING),
            );
        }
        let inputs = args.as_object().cloned().unwrap_or_default();
        let child = Frame {
            depth: frame.depth + 1,
            ..*frame
        };
        if let Some(builtin) = NamedWorkflow::parse(name) {
            return match builtin_inputs(builtin, &inputs) {
                Ok(input) => self.run_builtin(&child, builtin, input).await,
                Err(message) => failed("workflowInputs", message),
            };
        }
        let definition = match workflows::load(&self.workflow_dirs, name) {
            Ok((_, definition)) => definition,
            Err(LoadFailure::NotFound) => {
                return failed("workflowNotFound", format!("no workflow named `{name}`"))
            }
            Err(LoadFailure::Invalid(_, error)) => {
                return failed("workflowInvalid", error.to_string())
            }
        };
        match workflows::resolve_inputs(&definition, &inputs) {
            Ok(inputs) => self.run_file(&child, &definition, inputs, None).await.0,
            Err(message) => failed("workflowInputs", message),
        }
    }

    /// Runs `definition` from the step `resume` names, reusing the entries
    /// before it; returns the run value and its stored form.
    fn run_file<'a>(
        &'a self,
        frame: &'a Frame<'a>,
        definition: &'a Definition,
        inputs: Map<String, Value>,
        resume: Option<(usize, Vec<Value>)>,
    ) -> Pin<Box<dyn Future<Output = FileRun> + Send + 'a>> {
        Box::pin(async move {
            let mut run = Run::new(&definition.name, frame.workflow_id.clone());
            let mut results: BTreeMap<String, Value> = BTreeMap::new();
            let mut entries = Vec::new();
            let mut stopped_at = None;
            let (from, reused) = resume.unwrap_or_default();
            for (index, step) in definition.steps.iter().enumerate() {
                let mut entry = json!({"id":step.id});
                match (&step.tool, &step.workflow) {
                    (Some(tool), _) => entry["tool"] = json!(tool),
                    (None, Some(workflow)) => entry["workflow"] = json!(workflow),
                    (None, None) => {}
                }
                if index < from {
                    let kept = reused[index].clone();
                    results.insert(step.id.clone(), kept["result"].clone());
                    let mut shown = kept.clone();
                    shown["reused"] = json!(true);
                    run.steps.push(shown);
                    entries.push(kept);
                    continue;
                }
                let runs = step.when.as_ref().is_none_or(|when| {
                    workflows::truthy(&workflows::resolve(when, &inputs, &results, None))
                });
                let (result, stop) = if !runs {
                    (json!({"status":"skipped"}), None)
                } else if let Some(for_each) = &step.for_each {
                    let items = workflows::resolve(for_each, &inputs, &results, None);
                    let items = items.as_array().cloned().unwrap_or_default();
                    if items.len() > workflows::MAX_FOR_EACH_ITEMS {
                        let result = failed(
                            "forEachTooLong",
                            format!(
                                "forEach runs at most {} items",
                                workflows::MAX_FOR_EACH_ITEMS
                            ),
                        );
                        let stop = Some((result["status"].clone(), result["error"].clone()));
                        (result, stop)
                    } else {
                        let mut each = Vec::new();
                        let mut stop = None;
                        for item in &items {
                            let args = workflows::resolve(
                                &Value::Object(step.args.clone()),
                                &inputs,
                                &results,
                                Some(item),
                            );
                            let result = self.run_unit(frame, step, args).await;
                            let halt = failure_code(&result).is_some_and(|code| {
                                !step.continue_on.iter().any(|allowed| allowed == code)
                            });
                            if halt {
                                stop = Some((json!("failed"), result["error"].clone()));
                            }
                            each.push(result);
                            if halt {
                                break;
                            }
                        }
                        (Value::Array(each), stop)
                    }
                } else {
                    let args = workflows::resolve(
                        &Value::Object(step.args.clone()),
                        &inputs,
                        &results,
                        None,
                    );
                    let result = self.run_unit(frame, step, args).await;
                    let stop = failure_code(&result)
                        .filter(|code| !step.continue_on.iter().any(|allowed| allowed == code))
                        .map(|_| (result["status"].clone(), result["error"].clone()));
                    (result, stop)
                };
                run.hoist_screenshots(&result);
                entry["result"] = result.clone();
                run.steps.push(entry.clone());
                entries.push(entry);
                results.insert(step.id.clone(), result);
                if let Some((status, error)) = stop {
                    run.failure = Some((step.id.clone(), status, error));
                    stopped_at = Some(index);
                    break;
                }
            }
            let outputs: Map<String, Value> = definition
                .outputs
                .iter()
                .map(|(name, output)| {
                    (
                        name.clone(),
                        workflows::resolve(output, &inputs, &results, None),
                    )
                })
                .collect();
            let mut value = run.into_value();
            value["outputs"] = Value::Object(outputs);
            (value, entries, stopped_at)
        })
    }

    pub(super) async fn dispatch_workflow_run(&self, id: Value, arguments: Value) -> Value {
        let input: WorkflowRunArgs = match bounded_parse(arguments) {
            Ok(input) => input,
            Err(()) => return invalid_params_reason(id, "malformedArguments"),
        };
        let workflow_id = input.workflow_id.clone().unwrap_or_default();
        let scope =
            json!({"sessionId":input.session_id,"pageId":input.page_id,"workflowId":workflow_id});
        let calls = AtomicUsize::new(0);
        let frame = Frame {
            id: &id,
            scope: &scope,
            workflow_id: &workflow_id,
            depth: 0,
            calls: &calls,
        };
        if let Some(builtin) = NamedWorkflow::parse(&input.workflow) {
            if input.run_id.is_some() || input.from.is_some() {
                return workflow_error(
                    id,
                    "workflowRun",
                    "a built-in workflow runs from the start",
                );
            }
            let value = match builtin_inputs(builtin, &input.inputs) {
                Ok(inputs) => self.run_builtin(&frame, builtin, inputs).await,
                Err(message) => return workflow_error(id, "workflowInputs", message),
            };
            return self.tool_success(id, value).await;
        }
        let definition = match workflows::load(&self.workflow_dirs, &input.workflow) {
            Ok((_, definition)) => definition,
            Err(LoadFailure::NotFound) => {
                return workflow_error(
                    id,
                    "workflowNotFound",
                    format!("no built-in or workflow file named `{}`", input.workflow),
                )
            }
            Err(LoadFailure::Invalid(path, error)) => {
                let mut response = workflow_error(id, "workflowInvalid", error.to_string());
                response["error"]["data"]["workflowError"] = error.to_value();
                response["error"]["data"]["path"] = json!(path.display().to_string());
                return response;
            }
        };
        let inputs = match workflows::resolve_inputs(&definition, &input.inputs) {
            Ok(inputs) => inputs,
            Err(message) => return workflow_error(id, "workflowInputs", message),
        };
        let resume = match (&input.run_id, &input.from) {
            (None, None) => None,
            (Some(run_id), Some(from)) => match self.resume_point(&definition, run_id, from) {
                Ok(resume) => Some(resume),
                Err(message) => return workflow_error(id, "workflowRun", message),
            },
            _ => return workflow_error(id, "workflowRun", "runId and from go together"),
        };
        let run_id = input
            .run_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let (mut value, entries, stopped_at) =
            self.run_file(&frame, &definition, inputs, resume).await;
        value["runId"] = json!(run_id);
        self.store_run(StoredRun {
            run_id,
            workflow: definition.name.clone(),
            entries,
            stopped_at,
        });
        self.tool_success(id, value).await
    }

    /// The index of `from` and the entries before it, from this connection's
    /// stored run `run_id` of the same workflow.
    fn resume_point(
        &self,
        definition: &Definition,
        run_id: &str,
        from: &str,
    ) -> Result<(usize, Vec<Value>), String> {
        let runs = self.workflow_runs.lock().expect("workflow runs lock");
        let stored = runs
            .iter()
            .find(|run| run.run_id == run_id && run.workflow == definition.name)
            .ok_or_else(|| {
                format!(
                    "no run `{run_id}` of `{}` in this connection",
                    definition.name
                )
            })?;
        let index = definition
            .steps
            .iter()
            .position(|step| step.id == from)
            .ok_or_else(|| format!("`{from}` is not a step of `{}`", definition.name))?;
        let reachable = stored.stopped_at.unwrap_or(stored.entries.len());
        let ids_match = definition.steps[..index]
            .iter()
            .zip(&stored.entries)
            .all(|(step, entry)| entry["id"] == step.id.as_str());
        if index > reachable || index > stored.entries.len() || !ids_match {
            return Err(format!(
                "the run did not complete the steps before `{from}`"
            ));
        }
        Ok((index, stored.entries[..index].to_vec()))
    }

    fn store_run(&self, run: StoredRun) {
        let mut runs = self.workflow_runs.lock().expect("workflow runs lock");
        runs.retain(|stored| stored.run_id != run.run_id);
        if runs.len() == MAX_STORED_RUNS {
            runs.pop_front();
        }
        runs.push_back(run);
    }
}

/// An invalid-params response naming `reason`, with `detail` saying why.
fn workflow_error(id: Value, reason: &'static str, detail: impl Into<String>) -> Value {
    let mut response = invalid_params_reason(id, reason);
    response["error"]["data"]["detail"] = json!(detail.into());
    response
}
