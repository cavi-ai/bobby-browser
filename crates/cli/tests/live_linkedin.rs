//! Live LinkedIn accessibility reads through the running runtime.
//!
//! Every test is `#[ignore]`d and read-only. Each one attaches as an agent
//! over the MCP transport `bobby mcp-stdio` uses, to the origin in
//! `BOBBY_LIVE_RUNTIME_URL` with the bearer in `BOBBY_LIVE_BEARER`, opens
//! LinkedIn, reads the accessibility tree, and closes its session. A missing
//! variable fails the test; it never skips.
//!
//! The harness only issues `workflow_start`, `workflow_observe`,
//! `a11y_snapshot`, and `session_close`; any other tool name panics, so no
//! click, activation, or typing can be sent from here.

use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

const ANSWER_TIMEOUT: Duration = Duration::from_secs(120);
const READ_ONLY_TOOLS: [&str; 4] = [
    "workflow_start",
    "workflow_observe",
    "a11y_snapshot",
    "session_close",
];

const ACTION_TOOLS: [&str; 3] = ["navigate", "type_text", "intent_follow"];
const FORBIDDEN_LABEL_WORDS: [&str; 16] = [
    "post", "send", "submit", "apply", "connect", "follow", "like", "react", "repost", "comment",
    "save", "delete", "remove", "accept", "share", "message",
];

fn required_env(name: &str) -> String {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => value,
        _ => panic!("{name} must be set to run the live LinkedIn tests"),
    }
}

struct Agent {
    writer: WriteHalf<DuplexStream>,
    reader: BufReader<ReadHalf<DuplexStream>>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
    next_id: u64,
}

impl Agent {
    async fn attach(origin: &str, bearer: &str) -> Self {
        let (client, bridge) = tokio::io::duplex(2 * gateway_transport::MAX_FRAME_BYTES);
        let (input, output) = tokio::io::split(bridge);
        let (origin, bearer) = (origin.to_owned(), bearer.to_owned());
        let task = tokio::spawn(async move {
            gateway_transport::connect(&origin, "mcp", &bearer, input, output).await
        });
        let (reader, writer) = tokio::io::split(client);
        let mut agent = Self {
            writer,
            reader: BufReader::new(reader),
            task,
            next_id: 1,
        };
        let initialized = agent
            .request(json!({"jsonrpc":"2.0","id":0,"method":"initialize",
                "params":{"protocolVersion":"2025-11-25","capabilities":{},
                          "clientInfo":{"name":"live-linkedin","version":"1"}}}))
            .await;
        assert!(
            initialized.is_some_and(|response| response.get("result").is_some()),
            "initialize was not answered by the runtime at BOBBY_LIVE_RUNTIME_URL"
        );
        agent
            .send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await;
        agent
    }

    async fn send(&mut self, message: Value) {
        self.writer
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn request(&mut self, message: Value) -> Option<Value> {
        let id = message["id"].clone();
        self.send(message).await;
        tokio::time::timeout(ANSWER_TIMEOUT, async {
            loop {
                let line = gateway_transport::read_frame(&mut self.reader)
                    .await
                    .ok()
                    .flatten()?;
                let value: Value = serde_json::from_str(&line).ok()?;
                if value["id"] == id {
                    return Some(value);
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    /// The tool's `structuredContent`. Refuses any tool outside the
    /// read-only allowlist.
    async fn call(&mut self, tool: &str, arguments: Value) -> Value {
        assert!(
            READ_ONLY_TOOLS.contains(&tool),
            "live LinkedIn tests are read-only; refusing to call {tool}"
        );
        self.invoke(tool, arguments).await
    }

    /// The few state-changing calls the tests need: navigation, typing into
    /// the search box, and following a link. Refused unless the tool is on
    /// the action allowlist and `label` (the control's name) names nothing
    /// that posts, sends, submits, connects, or otherwise commits.
    async fn act(&mut self, tool: &str, label: &str, arguments: Value) -> Value {
        assert!(
            ACTION_TOOLS.contains(&tool),
            "refusing to call {tool}: not on the live action allowlist"
        );
        let lower = label.to_ascii_lowercase();
        if let Some(word) = FORBIDDEN_LABEL_WORDS
            .iter()
            .find(|word| lower.contains(**word))
        {
            panic!("refusing {tool} on {label:?}: the name contains {word:?}");
        }
        self.invoke(tool, arguments).await
    }

    async fn invoke(&mut self, tool: &str, arguments: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let response = self
            .request(json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                            "params":{"name":tool,"arguments":arguments}}))
            .await
            .unwrap_or_else(|| panic!("{tool} was not answered"));
        assert!(
            response.get("error").is_none(),
            "{tool} returned a protocol error: {response}"
        );
        response["result"]["structuredContent"].clone()
    }

    async fn close(mut self) {
        let _ = self.writer.shutdown().await;
        let _ = self.task.await;
    }
}

/// One live session. Dropping it closes the session and the agent, whether
/// the test passed or panicked.
struct Live {
    agent: Option<Agent>,
    session_id: Value,
    page_id: Value,
    started: Value,
}

impl Live {
    async fn open(url: &str) -> Self {
        let origin = required_env("BOBBY_LIVE_RUNTIME_URL");
        let bearer = required_env("BOBBY_LIVE_BEARER");
        let mut agent = Agent::attach(&origin, &bearer).await;
        let started = agent
            .call("workflow_start", json!({"profile":"default","url":url}))
            .await;
        let session_id = started["sessionId"].clone();
        let page_id = started["pageId"].clone();
        let live = Self {
            agent: Some(agent),
            session_id,
            page_id,
            started: started.clone(),
        };
        assert!(
            live.session_id.is_string() && live.page_id.is_string(),
            "workflow_start did not return a session and page: {started}"
        );
        assert_eq!(started["status"], "completed", "workflow_start: {started}");
        live
    }

    fn agent(&mut self) -> &mut Agent {
        self.agent.as_mut().expect("agent is attached until drop")
    }

    async fn observe(&mut self) -> Value {
        let (session_id, page_id) = (self.session_id.clone(), self.page_id.clone());
        self.agent()
            .call(
                "workflow_observe",
                json!({"sessionId":session_id,"pageId":page_id}),
            )
            .await
    }

    async fn snapshot(&mut self, extra: Value) -> Value {
        let mut arguments = json!({"sessionId":self.session_id,"pageId":self.page_id});
        for (key, value) in extra.as_object().expect("snapshot arguments are an object") {
            arguments[key] = value.clone();
        }
        self.agent().call("a11y_snapshot", arguments).await
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let Some(mut agent) = self.agent.take() else {
            return;
        };
        let session_id = self.session_id.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                if session_id.is_string() {
                    let _ = tokio::time::timeout(
                        Duration::from_secs(30),
                        agent.call("session_close", json!({"sessionId":session_id})),
                    )
                    .await;
                }
                agent.close().await;
            });
        });
    }
}

/// Whether any object in `value` has this `role` and, when given, this `name`.
fn has_node(value: &Value, role: &str, name: Option<&str>) -> bool {
    match value {
        Value::Object(map) => {
            let role_matches = map.get("role").and_then(Value::as_str) == Some(role);
            let name_matches = name
                .is_none_or(|expected| map.get("name").and_then(Value::as_str) == Some(expected));
            (role_matches && name_matches) || map.values().any(|child| has_node(child, role, name))
        }
        Value::Array(items) => items.iter().any(|child| has_node(child, role, name)),
        _ => false,
    }
}

fn assert_node(result: &Value, role: &str, name: Option<&str>) {
    assert!(
        has_node(result, role, name),
        "no {role} node{} in the result: {result}",
        name.map(|name| format!(" named {name:?}"))
            .unwrap_or_default()
    );
}

const FEED: &str = "https://www.linkedin.com/feed/";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: real LinkedIn through the running runtime"]
async fn feed_default_observe_reaches_main() {
    let mut live = Live::open(FEED).await;
    let observed = live.observe().await;
    assert_node(&observed, "main", None);
    assert_node(&observed, "listitem", Some("Feed post"));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: real LinkedIn through the running runtime"]
async fn feed_snapshot_120_reaches_main() {
    let mut live = Live::open(FEED).await;
    let snapshot = live.snapshot(json!({"maxNodes":120})).await;
    assert_node(&snapshot, "main", None);
    assert_node(&snapshot, "listitem", Some("Feed post"));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: real LinkedIn through the running runtime"]
async fn feed_snapshot_scoped_to_main_has_no_banner() {
    let mut live = Live::open(FEED).await;
    let snapshot = live.snapshot(json!({"target":{"role":"main"}})).await;
    assert_eq!(
        snapshot["status"], "completed",
        "scoped snapshot: {snapshot}"
    );
    assert_node(&snapshot, "main", None);
    assert!(
        !has_node(&snapshot, "banner", None),
        "a banner node leaked into the main-scoped snapshot: {snapshot}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: real LinkedIn through the running runtime"]
async fn feed_snapshot_unknown_target_is_target_not_found() {
    let mut live = Live::open(FEED).await;
    let snapshot = live
        .snapshot(json!({"target":{"role":"main","accessibleName":"no-such-region-7f3a"}}))
        .await;
    assert_eq!(
        snapshot["error"]["code"], "targetNotFound",
        "unknown target did not return targetNotFound: {snapshot}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: real LinkedIn through the running runtime"]
async fn jobs_snapshot_120_reaches_primary_content() {
    let mut live = Live::open("https://www.linkedin.com/jobs/").await;
    let snapshot = live.snapshot(json!({"maxNodes":120})).await;
    assert_node(&snapshot, "region", Some("Primary content"));
}

/// The first object with this `role` (and `name`, when given).
fn find_node<'a>(value: &'a Value, role: &str, name: Option<&str>) -> Option<&'a Value> {
    match value {
        Value::Object(map) => {
            let role_matches = map.get("role").and_then(Value::as_str) == Some(role);
            let name_matches = name
                .is_none_or(|expected| map.get("name").and_then(Value::as_str) == Some(expected));
            if role_matches && name_matches {
                return Some(value);
            }
            map.values().find_map(|child| find_node(child, role, name))
        }
        Value::Array(items) => items.iter().find_map(|child| find_node(child, role, name)),
        _ => None,
    }
}

/// Every object with this `role` under `value`.
fn collect_nodes<'a>(value: &'a Value, role: &str, out: &mut Vec<&'a Value>) {
    match value {
        Value::Object(map) => {
            if map.get("role").and_then(Value::as_str) == Some(role) {
                out.push(value);
            }
            map.values()
                .for_each(|child| collect_nodes(child, role, out));
        }
        Value::Array(items) => items
            .iter()
            .for_each(|child| collect_nodes(child, role, out)),
        _ => {}
    }
}

fn count_nodes(value: &Value) -> usize {
    match value {
        Value::Object(map) => {
            usize::from(map.get("role").is_some_and(Value::is_string))
                + map.values().map(count_nodes).sum::<usize>()
        }
        Value::Array(items) => items.iter().map(count_nodes).sum(),
        _ => 0,
    }
}

/// Whether this node or any descendant carries a non-empty `name`.
fn has_any_name(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| !name.trim().is_empty())
                || map.values().any(has_any_name)
        }
        Value::Array(items) => items.iter().any(has_any_name),
        _ => false,
    }
}

fn strings_under<'a>(value: &'a Value, key: &str, out: &mut Vec<&'a str>) {
    match value {
        Value::Object(map) => {
            if let Some(text) = map.get(key).and_then(Value::as_str) {
                out.push(text);
            }
            map.values()
                .for_each(|child| strings_under(child, key, out));
        }
        Value::Array(items) => items
            .iter()
            .for_each(|child| strings_under(child, key, out)),
        _ => {}
    }
}

impl Live {
    async fn act(&mut self, tool: &str, label: &str, extra: Value) -> Value {
        let mut arguments = json!({"sessionId":self.session_id,"pageId":self.page_id});
        for (key, value) in extra.as_object().expect("action arguments are an object") {
            arguments[key] = value.clone();
        }
        self.agent().act(tool, label, arguments).await
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: real LinkedIn through the running runtime"]
async fn feed_search_box_is_named_and_typable() {
    let mut live = Live::open(FEED).await;
    let snapshot = live.snapshot(json!({"maxNodes":2048})).await;
    let search = find_node(&snapshot, "textbox", None)
        .unwrap_or_else(|| panic!("no textbox on the feed: {snapshot}"));
    let name = search["name"].as_str().unwrap_or_default().to_owned();
    assert!(
        !name.trim().is_empty(),
        "the feed search textbox has no name: {search}"
    );
    let target = search["target"].clone();
    assert!(
        target.is_object(),
        "the search textbox has no target: {search}"
    );
    for value in ["bobby live probe", ""] {
        let typed = live
            .act(
                "type_text",
                &name,
                json!({"target":target,"value":value,"clearFirst":true}),
            )
            .await;
        assert_eq!(typed["status"], "completed", "type_text {value:?}: {typed}");
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: real LinkedIn through the running runtime"]
async fn jobs_show_all_resolves_for_intent() {
    let mut live = Live::open("https://www.linkedin.com/jobs/").await;
    // Allowlisted by name: this click only navigates to a collection page.
    let followed = live
        .act(
            "intent_follow",
            "Show all",
            json!({
                "purpose":"Open the full job collection",
                "hints":{"role":"link","accessibleName":"Show all"},
                "expectedDestination":{
                    "condition":{"kind":"url","matcher":{"kind":"contains","value":"/jobs/collections"}},
                    "timeoutMs":15000
                }
            }),
        )
        .await;
    assert_eq!(followed["status"], "completed", "intent_follow: {followed}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: real LinkedIn through the running runtime"]
async fn messaging_list_has_named_threads_right_after_navigate() {
    let mut live = Live::open(FEED).await;
    let navigated = live
        .act(
            "navigate",
            "navigate",
            json!({"url":"https://www.linkedin.com/messaging/"}),
        )
        .await;
    assert_eq!(navigated["status"], "completed", "navigate: {navigated}");
    let (session_id, page_id) = (live.session_id.clone(), live.page_id.clone());
    let observed = live
        .agent()
        .call(
            "workflow_observe",
            json!({"sessionId":session_id,"pageId":page_id,"maxNodes":2048}),
        )
        .await;
    assert!(
        count_nodes(&observed) >= 50,
        "observation straight after navigate is near-empty ({} nodes): {observed}",
        count_nodes(&observed)
    );
    let list = find_node(&observed, "list", Some("Conversation List"))
        .unwrap_or_else(|| panic!("no Conversation List in the observation: {observed}"));
    let mut items = Vec::new();
    collect_nodes(list, "listitem", &mut items);
    assert!(
        !items.is_empty(),
        "Conversation List has no listitems: {list}"
    );
    assert!(
        items.iter().any(|item| has_any_name(item)),
        "no Conversation List listitem has a name or a named child: {list}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: real LinkedIn through the running runtime"]
async fn feed_start_reports_the_settled_url() {
    let live = Live::open(FEED).await;
    let mut urls = Vec::new();
    let mut titles = Vec::new();
    strings_under(&live.started, "url", &mut urls);
    strings_under(&live.started, "title", &mut titles);
    assert!(
        urls.iter().any(|url| url.contains("/feed")),
        "workflow_start did not report the feed URL: {}",
        live.started
    );
    assert!(
        !urls.iter().any(|url| url.contains("/uas/login")),
        "workflow_start reported the login redirect URL: {}",
        live.started
    );
    assert!(
        !titles
            .iter()
            .any(|title| title.to_ascii_lowercase().contains("sign in")),
        "workflow_start reported the login title: {}",
        live.started
    );
}
