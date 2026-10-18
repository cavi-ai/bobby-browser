//! Lost MCP replies and interrupted owners must never submit an order twice.
//! Uses the production `bobby mcp-stdio` adapter and scoped shared owner.
#![cfg(unix)]

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use axum::{
    extract::State,
    response::Html,
    routing::{get, post},
    Router,
};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Notify, Semaphore},
};

const TIMEOUT: Duration = Duration::from_secs(90);
const ORDER_KEY: &str = "cli-recovery-order";

fn bobby() -> PathBuf {
    // Integration tests live under target/debug/deps. Build both production
    // binaries explicitly before running this ignored live suite.
    let path = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("bobby");
    assert!(
        path.is_file(),
        "build bobby and mcp-gateway before running CLI recovery tests"
    );
    assert!(
        path.with_file_name("mcp-gateway").is_file(),
        "build mcp-gateway before running CLI recovery tests"
    );
    path
}

struct Scope {
    root: tempfile::TempDir,
    selection: String,
    bearer: String,
}

impl Scope {
    fn command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(bobby());
        command
            .current_dir(self.root.path())
            .env("BOBBY_BROWSER_SCOPE_DIR", self.root.path())
            .env("BOBBY_BROWSER_CONFIG", self.root.path().join("config.toml"))
            .env(
                "BOBBY_BROWSER_BOOTSTRAP_ENV",
                self.root.path().join("bootstrap.env"),
            )
            .env("AUTOMATION_RUNTIME_BROWSER_SELECTION", &self.selection)
            .env_remove("BOBBY_RUNTIME_URL");
        for name in ["TOKEN", "PRINCIPAL", "CAPABILITIES", "EXPIRES_AT", "PRESET"] {
            command.env_remove(format!("AUTOMATION_RUNTIME_BOOTSTRAP_{name}"));
        }
        if std::env::var("BOBBY_FIREFOX_HEADLESS").as_deref() == Ok("1") {
            command.env("MOZ_HEADLESS", "1");
        }
        command
    }

    async fn runtime(&self, args: &[&str]) {
        let output = tokio::time::timeout(
            TIMEOUT,
            Command::from(self.command())
                .args(args)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn owner(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.root.path().join("runtime/owner.json")).unwrap())
            .unwrap()
    }

    fn http(&self, method: reqwest::Method, route: &str, bearer: &str) -> reqwest::RequestBuilder {
        let origin = self.owner()["url"].as_str().unwrap().to_owned();
        reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .unwrap()
            .request(method, format!("{origin}{route}"))
            .bearer_auth(bearer)
            .header("x-interface-version", types::CURRENT_INTERFACE_VERSION)
            .header("x-correlation-id", uuid::Uuid::new_v4().to_string())
            .header(
                "x-deadline",
                (chrono::Utc::now() + chrono::Duration::minutes(2)).to_rfc3339(),
            )
    }

    async fn unauthorized_recovery(&self, workflow: &Value) {
        let route = format!("/v1/recovery/{}", workflow.as_str().unwrap());
        let authorized = self
            .http(reqwest::Method::GET, &route, &self.bearer)
            .send()
            .await
            .unwrap();
        assert_eq!(
            authorized.status(),
            reqwest::StatusCode::OK,
            "the checkpoint must exist before testing access isolation"
        );
        let status: types::RecoveryStatus = authorized.json().await.unwrap();
        assert_eq!(serde_json::to_value(status.workflow_id).unwrap(), *workflow);
        for (capabilities, expected_status) in [
            (vec!["session:read"], reqwest::StatusCode::FORBIDDEN),
            (vec!["recovery:read"], reqwest::StatusCode::NOT_FOUND),
        ] {
            let response = self.http(reqwest::Method::POST, "/v1/principals", &self.bearer)
                .header("idempotency-key", uuid::Uuid::new_v4().to_string())
                .json(&json!({"principalId":uuid::Uuid::new_v4(), "capabilities":capabilities,"expiresAt":(chrono::Utc::now()+chrono::Duration::minutes(10)).to_rfc3339()}))
                .send().await.unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::CREATED);
            let issued: Value = response.json().await.unwrap();
            let response = self
                .http(
                    reqwest::Method::GET,
                    &route,
                    issued["bearer"].as_str().unwrap(),
                )
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), expected_status);
        }
    }

    async fn order_state(&self) -> Value {
        let ledger = tokio::fs::read_to_string(self.root.path().join("commands.idempotency.json"))
            .await
            .unwrap();
        ledger
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .flat_map(|record| {
                record["entries"].as_array().cloned().unwrap_or_else(|| {
                    record["change"]["updates"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                })
            })
            .rfind(|entry| entry["key"] == ORDER_KEY)
            .expect("accepted order must have a durable idempotency reservation")["state"]
            .clone()
    }

    async fn wait_for_retained_completion(&self) {
        // Observe durability without consuming the adapter's reply. JSONL may
        // be read during an append; ignore incomplete lines and retry.
        tokio::time::timeout(TIMEOUT, async {
            loop {
                let state = self.order_state().await;
                if state["kind"] == "retained" {
                    let command = &state["value"];
                    let journal =
                        tokio::fs::read_to_string(self.root.path().join("commands.jsonl"))
                            .await
                            .unwrap();
                    for record in journal.lines().filter_map(|line| {
                        serde_json::from_str::<workflow_journal::JournalRecord>(line).ok()
                    }) {
                        if serde_json::to_value(&record.command_id).unwrap() == *command {
                            if let Some(outcome) = record.outcome {
                                assert!(
                                    matches!(outcome, types::CommandOutcome::Completed { .. }),
                                    "submit did not complete: {outcome:?}"
                                );
                                return;
                            }
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("owner did not retain the completed submit before adapter disconnect");
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        let mut command = self.command();
        let _ = command
            .args(["runtime", "stop", "--disconnect-agents"])
            .output();
        // A force-killed owner may leave its browser alive. Reap only processes
        // bearing this test's exact, unique profile directory.
        let profile = self.root.path().join("firefox");
        let needle = format!("--profile {} ", profile.display());
        if let Ok(output) = std::process::Command::new("ps")
            .args(["-ax", "-o", "pid=,command="])
            .output()
        {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                if line.to_lowercase().contains("firefox") && line.contains(&needle) {
                    if let Some(pid) = line
                        .split_whitespace()
                        .next()
                        .and_then(|pid| pid.parse::<i32>().ok())
                    {
                        unsafe {
                            libc::kill(pid, libc::SIGKILL);
                        }
                    }
                }
            }
        }
    }
}

fn config(root: &Path) -> config::AppConfig {
    let mut config = config::AppConfig::default();
    config.http.allow_loopback = true;
    config.server.port = 0;
    config.browser.headless = true;
    config.browser.profiles_dir = root.join("profiles");
    config.browser.downloads_dir = root.join("downloads");
    config.browser.artifacts_dir = root.join("artifacts");
    config.browser.executable = std::env::var_os("BOBBY_CHROME_EXECUTABLE").map(PathBuf::from);
    config.storage.journal_path = root.join("commands.jsonl");
    config.storage.authority_path = root.join("authority.json");
    config.storage.checkpoints_dir = root.join("checkpoints");
    config.storage.scheduler_journal_path = root.join("jobs.jsonl");
    config
}

fn copy_dir(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target.join(entry.file_name()));
        } else {
            std::fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
        }
    }
}

async fn scope(firefox: bool) -> (Scope, Option<runtime_tests::InstalledFirefoxRuntime>) {
    let root = tempfile::tempdir().unwrap();
    let config = config(root.path());
    std::fs::write(
        root.path().join("config.toml"),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();
    let bearer = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let capabilities = types::CapabilityPreset::Unrestricted
        .capabilities()
        .iter()
        .map(|cap| cap.as_str())
        .collect::<Vec<_>>()
        .join(",");
    std::fs::write(root.path().join("bootstrap.env"), format!("# bobby-bootstrap-preset: unrestricted\nAUTOMATION_RUNTIME_BOOTSTRAP_TOKEN={bearer}\nAUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL={}\nAUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES={capabilities}\nAUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT={}\n", uuid::Uuid::new_v4(), (chrono::Utc::now()+chrono::Duration::hours(1)).to_rfc3339())).unwrap();
    let mut scope = Scope {
        root,
        selection: r#"{"preference":{"mode":"managedChromium"},"firefox":[]}"#.into(),
        bearer,
    };
    if !firefox {
        return (scope, None);
    }
    let mut installed = runtime_tests::InstalledFirefoxConfig::from_env().unwrap();
    installed.profile = scope.root.path().join("firefox");
    std::fs::create_dir_all(&installed.profile).unwrap();
    std::fs::write(
        installed.profile.join("user.js"),
        concat!(
            "user_pref(\"xpinstall.signatures.required\", false);\n",
            "user_pref(\"extensions.autoDisableScopes\", 14);\n",
            "user_pref(\"extensions.startupScanScopes\", 1);\n",
            "user_pref(\"browser.shell.checkDefaultBrowser\", false);\n",
            "user_pref(\"browser.aboutwelcome.enabled\", false);\n",
        ),
    )
    .unwrap();
    copy_dir(
        &installed.companion_extension,
        &installed
            .profile
            .join("extensions/firefox-companion@bobby-browser.local"),
    );
    let firefox =
        runtime_tests::launch_installed_firefox_runtime(installed, &config, "about:blank")
            .await
            .unwrap();
    scope.selection = serde_json::to_string(&firefox.browser_selection()).unwrap();
    // Keep the enrolled browser alive while transferring its lease to the owner.
    firefox.factory().shutdown().await;
    (scope, Some(firefox))
}

struct Shop {
    orders: AtomicUsize,
    received: Notify,
    release: Semaphore,
}

async fn shop_page(State(shop): State<Arc<Shop>>) -> Html<String> {
    Html(format!("<!doctype html><title>Shop</title><main><h1>Checkout</h1><p>Orders placed: {}</p><form method=\"post\" action=\"/order\"><button type=\"submit\">Place order</button></form></main>", shop.orders.load(Ordering::SeqCst)))
}

async fn place_order(State(shop): State<Arc<Shop>>) -> Html<&'static str> {
    shop.orders.fetch_add(1, Ordering::SeqCst);
    shop.received.notify_one();
    shop.release.acquire().await.unwrap().forget();
    Html("<!doctype html><title>Confirmed</title><main>Order confirmed</main>")
}

struct Site {
    shop: Arc<Shop>,
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Site {
    async fn start() -> Self {
        let shop = Arc::new(Shop {
            orders: AtomicUsize::new(0),
            received: Notify::new(),
            release: Semaphore::new(0),
        });
        let app = Router::new()
            .route("/shop", get(shop_page))
            .route("/order", post(place_order))
            .with_state(shop.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/shop", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { shop, url, task }
    }
}
impl Drop for Site {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Adapter {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

impl Adapter {
    async fn start(scope: &Scope) -> Self {
        let mut child = Command::from(scope.command())
            .arg("mcp-stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut adapter = Self {
            child,
            stdin,
            lines,
            next_id: 0,
        };
        let initialized = adapter.request("initialize", json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"cli-recovery","version":"1"}})).await;
        assert!(initialized.get("result").is_some(), "{initialized}");
        adapter
            .send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await;
        adapter
    }

    async fn send(&mut self, message: Value) {
        self.stdin
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await;
        self.response(id).await
    }
    async fn send_call(&mut self, tool: &str, arguments: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":tool,"arguments":arguments}})).await;
        id
    }
    async fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let id = self.send_call(tool, arguments).await;
        self.response(id).await
    }
    async fn response(&mut self, id: u64) -> Value {
        tokio::time::timeout(TIMEOUT, async {
            loop {
                let line = self
                    .lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("adapter stdout closed");
                let value: Value =
                    serde_json::from_str(&line).expect("production stdout is JSON-RPC");
                if value["id"] == id {
                    return value;
                }
            }
        })
        .await
        .expect("adapter response timed out")
    }
    async fn disconnect(mut self) {
        self.child.kill().await.unwrap();
        self.child.wait().await.unwrap();
    }
}

fn submit(handle: &Value) -> Value {
    json!({"workflowHandle":handle,"idempotencyKey":ORDER_KEY,"purpose":"Place the order","hints":{"role":"button","accessibleName":"Place order"},"expectedState":{"condition":{"kind":"url","matcher":{"kind":"contains","value":"/order"}},"timeoutMs":30000}})
}

async fn open_shop(adapter: &mut Adapter, url: &str) -> Value {
    let response = adapter
        .call(
            "workflow_start",
            json!({"profile":"recovery-shop","url":url,"includePostState":true}),
        )
        .await;
    let workflow = response["result"]["structuredContent"].clone();
    assert!(workflow["workflowHandle"].is_string(), "{response}");
    assert_eq!(
        workflow["postStateStatus"]["status"], "available",
        "{response}"
    );
    for field in ["workflowHandle", "sessionId", "pageId", "workflowId"] {
        assert_eq!(
            workflow["postState"][field], workflow[field],
            "{field}: {response}"
        );
    }
    assert_eq!(workflow["postState"]["pageDerived"], true, "{response}");
    assert!(
        workflow["postState"].to_string().contains("Place order"),
        "{response}"
    );
    workflow
}

async fn recovery_case(firefox: bool, kill_owner: bool) {
    let site = Site::start().await;
    let (scope, _firefox) = scope(firefox).await;
    let mut first = Adapter::start(&scope).await;
    let owner = scope.owner();
    let original = open_shop(&mut first, &site.url).await;
    let request = first
        .send_call(
            "intent_submit_and_verify",
            submit(&original["workflowHandle"]),
        )
        .await;
    tokio::select! {
        () = site.shop.received.notified() => {},
        response = first.response(request) => panic!("submit answered before fixture accepted the order: {response}"),
        () = tokio::time::sleep(TIMEOUT) => panic!("shop did not receive order"),
    }
    assert_eq!(site.shop.orders.load(Ordering::SeqCst), 1);
    scope.unauthorized_recovery(&original["workflowId"]).await;
    if kill_owner {
        assert_eq!(scope.order_state().await["kind"], "reserved");
        scope
            .runtime(&["runtime", "restart", "--force", "--disconnect-agents"])
            .await;
        assert_ne!(scope.owner()["owner_id"], owner["owner_id"]);
        assert_ne!(
            scope.order_state().await["kind"],
            "retained",
            "owner must be interrupted before recording a terminal outcome"
        );
    } else {
        site.shop.release.add_permits(1);
        scope.wait_for_retained_completion().await;
    }
    first.disconnect().await;
    if kill_owner {
        site.shop.release.add_permits(1);
    }

    let mut second = Adapter::start(&scope).await;
    if !kill_owner {
        assert_eq!(scope.owner()["owner_id"], owner["owner_id"]);
    }
    let stale = second
        .call(
            "workflow_observe",
            json!({"workflowHandle":original["workflowHandle"]}),
        )
        .await;
    assert_eq!(
        stale["error"]["data"]["reason"], "unknownWorkflowHandle",
        "{stale}"
    );
    let workflow = open_shop(&mut second, &site.url).await;
    let retry = second
        .call(
            "intent_submit_and_verify",
            submit(&workflow["workflowHandle"]),
        )
        .await;
    let error = &retry["error"]["data"]["interfaceError"];
    assert_eq!(error["code"], "idempotencyConflict", "{retry}");
    assert_eq!(error["reconciliationRequired"], kill_owner, "{retry}");
    assert_eq!(error["retryable"], false, "{retry}");
    let observed = second
        .call(
            "workflow_observe",
            json!({"workflowHandle":workflow["workflowHandle"],"goal":"Orders placed"}),
        )
        .await;
    assert!(
        observed["result"].to_string().contains("Orders placed: 1"),
        "{observed}"
    );
    assert_eq!(
        site.shop.orders.load(Ordering::SeqCst),
        1,
        "reconnect must not submit another order"
    );
    second.disconnect().await;
    scope
        .runtime(&["runtime", "stop", "--disconnect-agents"])
        .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium and built production CLI binaries"]
async fn chromium_adapter_disconnect_preserves_one_order() {
    recovery_case(false, false).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium and built production CLI binaries"]
async fn chromium_owner_restart_requires_reconciliation() {
    recovery_case(false, true).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires enrolled Firefox test setup and built production CLI binaries"]
async fn firefox_adapter_disconnect_preserves_one_order() {
    recovery_case(true, false).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires enrolled Firefox test setup and built production CLI binaries"]
async fn firefox_owner_restart_requires_reconciliation() {
    recovery_case(true, true).await;
}
