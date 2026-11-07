//! `workflow_run`: named workflows that run existing tools in a fixed order
//! inside one call. Each step goes through `call_tool`, so it keeps that
//! tool's authorization, validation, guards and evidence.

use super::*;
use crate::schema::{MAX_WORKFLOW_LINKS, MAX_WORKFLOW_PAGES};
use crate::tool_args::{NamedWorkflow, WorkflowRunArgs};

/// The settle wait `collectLinks` uses after opening the next page when
/// `next` names none.
fn next_page_settled() -> Value {
    json!({"condition":{"kind":"networkQuiet","idleMs":200,"maxInFlight":0},"timeoutMs":10000})
}

struct Run {
    workflow: NamedWorkflow,
    workflow_id: types::WorkflowId,
    scope: Value,
    steps: Vec<Value>,
    screenshots: Vec<Value>,
    failure: Option<(String, Value)>,
}

impl Run {
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

    fn record(&mut self, tool: &str, result: Value) {
        if result["status"] != "completed" {
            self.failure = Some((tool.to_owned(), result.clone()));
        }
        if let Some(evidence) = result["evidence"].as_array() {
            self.screenshots.extend(
                evidence
                    .iter()
                    .filter(|item| item["kind"] == "screenshot")
                    .cloned(),
            );
        }
        self.steps.push(json!({"tool":tool,"result":result}));
    }

    fn into_value(self, collected: Option<(Vec<Value>, u32)>) -> Value {
        let mut value = json!({
            "status":"completed",
            "workflow":self.workflow.as_str(),
            "workflowId":self.workflow_id,
            "steps":self.steps,
            "evidence":self.screenshots
        });
        if let Some((tool, result)) = self.failure {
            value["status"] = result["status"].clone();
            value["stoppedAt"] = json!(tool);
            if let Some(error) = result.get("error") {
                value["error"] = error.clone();
            }
        }
        if let Some((links, pages)) = collected {
            value["links"] = json!(links);
            value["pages"] = json!(pages);
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

impl Server {
    async fn run_step(&self, id: &Value, run: &Run, tool: &str, arguments: Value) -> Value {
        let mut merged = run.scope.clone();
        for (key, value) in arguments.as_object().into_iter().flatten() {
            merged[key] = value.clone();
        }
        let params = json!({"name":tool,"arguments":merged});
        Run::outcome(&self.call_step(id.clone(), params).await)
    }

    /// A named return type keeps the recursion through `call_tool` from
    /// making its future's `Send` depend on itself.
    fn call_step(
        &self,
        id: Value,
        params: Value,
    ) -> Pin<Box<dyn Future<Output = Value> + Send + '_>> {
        Box::pin(self.call_tool(id, params))
    }

    async fn step(&self, id: &Value, run: &mut Run, tool: &str, arguments: Value) -> bool {
        let result = self.run_step(id, run, tool, arguments).await;
        run.record(tool, result);
        !run.failed()
    }

    pub(super) async fn dispatch_workflow_run(&self, id: Value, arguments: Value) -> Value {
        let input: WorkflowRunArgs = match bounded_parse(arguments) {
            Ok(input) => input,
            Err(()) => return invalid_params_reason(id, "malformedArguments"),
        };
        let workflow_id = input.workflow_id.clone().unwrap_or_default();
        let mut run = Run {
            workflow: input.workflow,
            scope: json!({"sessionId":input.session_id,"pageId":input.page_id,"workflowId":workflow_id}),
            workflow_id,
            steps: Vec::new(),
            screenshots: Vec::new(),
            failure: None,
        };
        let missing = match input.workflow {
            NamedWorkflow::FillForm | NamedWorkflow::ExtractPage => {
                input.fields.is_none().then_some("workflowNeedsFields")
            }
            NamedWorkflow::CollectLinks => input.list.is_none().then_some("workflowNeedsList"),
        };
        if let Some(reason) = missing {
            return invalid_params_reason(id, reason);
        }
        let mut collected = None;
        let opened = match &input.url {
            Some(url) => {
                self.step(&id, &mut run, "navigate", json!({"url":url}))
                    .await
            }
            None => true,
        };
        match input.workflow {
            NamedWorkflow::FillForm => {
                let fields = input.fields.unwrap_or_default();
                let mut fill = json!({"purpose":"fill the form","fields":fields});
                if let Some(screenshot) = input.screenshot {
                    fill["screenshot"] = json!(screenshot);
                }
                if opened && self.step(&id, &mut run, "intent_complete_form", fill).await {
                    if let Some(mut submit) = input.submit {
                        if let Some(screenshot) = input.screenshot {
                            submit["screenshot"] = json!(screenshot);
                        }
                        self.step(&id, &mut run, "intent_submit_and_verify", submit)
                            .await;
                    }
                }
            }
            NamedWorkflow::ExtractPage => {
                let fields = input.fields.unwrap_or_default();
                if opened {
                    self.step(
                        &id,
                        &mut run,
                        "intent_extract",
                        json!({"purpose":"read the page","fields":fields}),
                    )
                    .await;
                }
            }
            NamedWorkflow::CollectLinks => {
                let list = input.list.unwrap_or_default();
                let max_pages = input.max_pages.unwrap_or(1).clamp(1, MAX_WORKFLOW_PAGES);
                let mut links: Vec<(String, String)> = Vec::new();
                let mut pages = 0;
                while opened && pages < max_pages {
                    let snapshot = self
                        .run_step(&id, &run, "a11y_snapshot", json!({"target":list}))
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
                    if follow.get("expectedState").is_none() {
                        follow["expectedState"] = next_page_settled();
                    }
                    let followed = self.run_step(&id, &run, "intent_follow", follow).await;
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
                let links = links
                    .into_iter()
                    .map(|(name, url)| json!({"name":name,"url":url}))
                    .collect();
                collected = Some((links, pages));
            }
        }
        self.tool_success(id, run.into_value(collected)).await
    }
}
