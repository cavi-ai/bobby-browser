//! Many agents on one shared runtime owner, on real Chromium.
//!
//! `bobby runtime start` launches the production owner process for a temp
//! scope. Each agent then attaches over the same MCP transport `bobby
//! mcp-stdio` uses and runs one journey: `workflow_start` on the test site,
//! `intent_complete_form` on its Name field, `session_close`. Levels of 1, 4,
//! and 8 concurrent agents (the default `max_active` browser capacity) run one
//! after another against the same owner.
//!
//! Every request must be answered and every journey must complete. Latencies
//! are printed, not asserted; run with `--nocapture` for the table the docs
//! reproduce.
//!
//! Every connection holds one of its principal's `max_in_flight_per_principal`
//! permits for as long as it is open. With that many agents attached under the
//! one bootstrap principal, the next is refused and told why.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

const LEVELS: [usize; 3] = [1, 4, 8];
const ANSWER_TIMEOUT: Duration = Duration::from_secs(120);

fn chrome() -> PathBuf {
    std::env::var("BOBBY_CHROME_EXECUTABLE")
        .map(Into::into)
        .unwrap_or_else(|_| "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into())
}

fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bobby"));
    command
        .env("BOBBY_BROWSER_SCOPE_DIR", root)
        .env("BOBBY_BROWSER_CONFIG", root.join("config.toml"))
        .env("BOBBY_BROWSER_BOOTSTRAP_ENV", root.join("bootstrap.env"))
        .env(
            "AUTOMATION_RUNTIME_BROWSER_SELECTION",
            r#"{"preference":{"mode":"managedChromium"},"firefox":[]}"#,
        )
        .env_remove("BOBBY_RUNTIME_URL")
        .env_remove("AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN")
        .env_remove("AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL")
        .env_remove("AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES")
        .env_remove("AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT")
        .env_remove("AUTOMATION_RUNTIME_BOOTSTRAP_PRESET");
    command
}

fn checked(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct Owner(tempfile::TempDir);

impl Drop for Owner {
    fn drop(&mut self) {
        let _ = command(self.0.path())
            .args(["runtime", "stop", "--disconnect-agents"])
            .output();
    }
}

/// One MCP request as an agent saw it.
#[derive(Debug)]
struct Sample {
    tool: &'static str,
    millis: u128,
    answered: bool,
    completed: bool,
}

struct Agent {
    writer: WriteHalf<DuplexStream>,
    reader: BufReader<ReadHalf<DuplexStream>>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
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
        };
        let initialized = agent
            .request(json!({"jsonrpc":"2.0","id":0,"method":"initialize",
                "params":{"protocolVersion":"2025-11-25","capabilities":{},
                          "clientInfo":{"name":"shared-runtime-load","version":"1"}}}))
            .await;
        assert!(
            initialized.is_some_and(|response| response.get("result").is_some()),
            "initialize was not answered"
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

    /// The response to `message`, or `None` when none arrives in time.
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

    async fn call(&mut self, id: u64, tool: &'static str, arguments: Value) -> (Sample, Value) {
        let started = Instant::now();
        let response = self
            .request(json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                            "params":{"name":tool,"arguments":arguments}}))
            .await;
        let millis = started.elapsed().as_millis();
        let result = response
            .as_ref()
            .map(|response| response["result"]["structuredContent"].clone())
            .unwrap_or(Value::Null);
        let completed = response.as_ref().is_some_and(|response| {
            response.get("error").is_none() && response["result"]["isError"] != true
        }) && (tool == "session_close" || result["status"] == "completed");
        (
            Sample {
                tool,
                millis,
                answered: response.is_some(),
                completed,
            },
            result,
        )
    }

    async fn journey(mut self, url: String, index: usize) -> Vec<Sample> {
        let mut samples = Vec::new();
        let (sample, started) = self
            .call(
                1,
                "workflow_start",
                json!({"profile":format!("load-{index}"),"url":url}),
            )
            .await;
        samples.push(sample);
        if samples[0].completed {
            let ids = json!({"sessionId":started["sessionId"],"pageId":started["pageId"]});
            let (sample, _) = self
                .call(
                    2,
                    "intent_complete_form",
                    json!({
                        "sessionId":ids["sessionId"],"pageId":ids["pageId"],
                        "purpose":"Name",
                        "fields":[{"name":"name","purpose":"Name",
                            "hints":{"role":"textbox","accessibleName":"Name"},
                            "value":{"kind":"setText","value":format!("Agent {index}"),"clearFirst":true}}]
                    }),
                )
                .await;
            samples.push(sample);
            let (sample, _) = self
                .call(3, "session_close", json!({"sessionId":ids["sessionId"]}))
                .await;
            samples.push(sample);
        }
        self.close().await;
        samples
    }

    async fn close(mut self) {
        self.writer.shutdown().await.unwrap();
        let _ = self.task.await;
    }
}

fn percentile(sorted: &[u128], fraction: f64) -> u128 {
    let rank = ((sorted.len() as f64) * fraction).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chrome or Chromium"]
async fn eight_agents_share_one_runtime_owner_without_losing_a_request() {
    let site = test_site::spawn().await;
    let owner = Owner(tempfile::tempdir().unwrap());
    let root = owner.0.path();
    let mut config = config::AppConfig::default();
    config.http.allow_loopback = true;
    config.browser.executable = Some(chrome());
    config.browser.headless = true;
    config.browser.profiles_dir = root.join("profiles");
    config.browser.downloads_dir = root.join("downloads");
    config.browser.artifacts_dir = root.join("artifacts");
    config.browser.upload_roots = Vec::new();
    config.storage.journal_path = root.join("storage/commands.jsonl");
    config.storage.checkpoints_dir = root.join("storage/checkpoints");
    config.storage.authority_path = root.join("storage/authority.json");
    config.storage.scheduler_journal_path = root.join("storage/scheduler-jobs.jsonl");
    let capacity = config.browser.max_active;
    assert_eq!(
        capacity,
        *LEVELS.last().unwrap(),
        "levels track the default capacity"
    );
    std::fs::write(root.join("config.toml"), toml::to_string(&config).unwrap()).unwrap();
    let origin = checked(command(root).args(["runtime", "start"]).output().unwrap());
    let bearer = std::fs::read_to_string(root.join("bootstrap.env"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN="))
        .unwrap()
        .trim_matches('"')
        .to_owned();

    println!("| Agents | Journeys completed | Requests lost | Wall s | workflow_start p50 / p95 ms | intent_complete_form p50 / p95 ms | session_close p50 / p95 ms |");
    for agents in LEVELS {
        let started = Instant::now();
        let mut journeys = tokio::task::JoinSet::new();
        for index in 0..agents {
            let agent = Agent::attach(&origin, &bearer).await;
            journeys.spawn(agent.journey(site.base_url(), index));
        }
        let mut completed = 0;
        let mut lost = 0;
        let mut samples = Vec::new();
        while let Some(journey) = journeys.join_next().await {
            let journey = journey.unwrap();
            if journey.len() == 3 && journey.iter().all(|sample| sample.completed) {
                completed += 1;
            }
            lost += journey.iter().filter(|sample| !sample.answered).count();
            samples.extend(journey);
        }
        let wall = started.elapsed().as_secs_f64();
        let mut row = format!("| {agents} | {completed}/{agents} | {lost} | {wall:.1} |");
        for tool in ["workflow_start", "intent_complete_form", "session_close"] {
            let mut millis = samples
                .iter()
                .filter(|sample| sample.tool == tool)
                .map(|sample| sample.millis)
                .collect::<Vec<_>>();
            millis.sort_unstable();
            if millis.is_empty() {
                row.push_str(" – |");
            } else {
                row.push_str(&format!(
                    " {} / {} |",
                    percentile(&millis, 0.5),
                    percentile(&millis, 0.95)
                ));
            }
        }
        println!("{row}");
        assert_eq!(lost, 0, "{agents} agents: unanswered requests {samples:?}");
        assert_eq!(completed, agents, "{agents} agents: {samples:?}");
    }

    let quota = config.interface.max_in_flight_per_principal;
    let mut held = Vec::new();
    for _ in 0..quota {
        held.push(Agent::attach(&origin, &bearer).await);
    }
    let refused = gateway_transport::connect_within(
        &origin,
        "mcp",
        &bearer,
        tokio::io::empty(),
        tokio::io::sink(),
        std::time::Duration::ZERO,
    )
    .await
    .unwrap_err()
    .to_string();
    println!("connection {} of one principal: {refused}", quota + 1);
    assert!(
        refused.contains("resourceExhausted") && refused.contains("retry after"),
        "{refused}"
    );
    for agent in held {
        agent.close().await;
    }
    // The held agents were just closed; their connections and sessions end
    // asynchronously, so the stop must not depend on that having finished.
    checked(
        command(root)
            .args(["runtime", "stop", "--disconnect-agents"])
            .output()
            .unwrap(),
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chrome or Chromium"]
async fn stop_without_the_flag_succeeds_after_an_agent_leaves_with_a_session_open() {
    let owner = Owner(tempfile::tempdir().unwrap());
    let root = owner.0.path();
    let mut config = config::AppConfig::default();
    config.http.allow_loopback = true;
    config.browser.executable = Some(chrome());
    config.browser.headless = true;
    config.browser.profiles_dir = root.join("profiles");
    config.browser.downloads_dir = root.join("downloads");
    config.browser.artifacts_dir = root.join("artifacts");
    config.browser.upload_roots = Vec::new();
    config.storage.journal_path = root.join("storage/commands.jsonl");
    config.storage.checkpoints_dir = root.join("storage/checkpoints");
    config.storage.authority_path = root.join("storage/authority.json");
    config.storage.scheduler_journal_path = root.join("storage/scheduler-jobs.jsonl");
    std::fs::write(root.join("config.toml"), toml::to_string(&config).unwrap()).unwrap();
    let origin = checked(command(root).args(["runtime", "start"]).output().unwrap());
    let bearer = std::fs::read_to_string(root.join("bootstrap.env"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN="))
        .unwrap()
        .trim_matches('"')
        .to_owned();

    let mut agent = Agent::attach(&origin, &bearer).await;
    let (sample, created) = agent
        .call(1, "session_create", json!({"profile":"leaves-open"}))
        .await;
    assert!(
        sample.answered && created["id"].is_string(),
        "session_create: {created}"
    );
    agent.close().await;

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let output = command(root).args(["runtime", "stop"]).output().unwrap();
        if output.status.success() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "stop without --disconnect-agents still refused 20 s after the agent left: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
