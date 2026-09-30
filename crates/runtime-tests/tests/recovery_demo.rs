//! Recovery demo: kill the MCP gateway mid-submit, start it again on the same
//! data directory, and the shop records one order, not two.
//!
//! The parent test hosts a shop whose `POST /order` counts the order and then
//! never answers, so the submit is still in flight when the gateway dies. The
//! gateway is this test binary re-run as an MCP stdio child
//! (`recovery_demo_gateway`) on real Chromium, with one fixed principal, so
//! both gateway processes share the durable command journal and idempotency
//! ledger the way two runs of `bobby mcp` on one install do.
//!
//! After the restart the agent's old session is gone. It opens a new one and
//! resends the order under the same idempotency key: the runtime refuses it as
//! unresolved and tells the agent to check for the effect instead of minting a
//! fresh key. The agent reads the page, sees one order, and stops.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration as StdDuration;

use axum::{extract::State, response::Html, routing::get, Router};
use chrono::{Duration, Utc};
use interface_core::AuthorityStore;
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Notify;
use types::{Capability, PrincipalId};
use worker_pool::ChromiumWorkerFactory;

const ROOT_ENV: &str = "RECOVERY_DEMO_ROOT";
const PRINCIPAL: uuid::Uuid = uuid::uuid!("10000000-0000-4000-8000-00000000d3e0");
const ORDER_KEY: &str = "order-2026-0042";

fn chrome_executable() -> PathBuf {
    std::env::var("BOBBY_CHROME_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        })
}

fn config(root: &Path) -> config::AppConfig {
    config::AppConfig {
        http: config::HttpConfig {
            allow_loopback: true,
            ..config::HttpConfig::default()
        },
        server: config::ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            shutdown_timeout_ms: 10_000,
        },
        browser: config::BrowserConfig {
            executable: Some(chrome_executable()),
            profiles_dir: root.join("profiles"),
            headless: true,
            max_active: 1,
            upload_roots: vec![],
            downloads_dir: root.join("downloads"),
            artifacts_dir: root.join("artifacts"),
            max_artifact_bytes: 8 * 1024 * 1024,
            max_screenshot_dimension: 16_384,
            max_js_result_bytes: 64 * 1024,
            max_js_timeout_ms: 30_000,
        },
        storage: config::StorageConfig {
            journal_path: root.join("commands.jsonl"),
            checkpoints_dir: root.join("checkpoints"),
            authority_path: root.join("authority.json"),
            scheduler_journal_path: root.join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
        context: config::ContextConfig::default(),
        nodes: Default::default(),
    }
}

/// The MCP stdio gateway the parent test starts, kills, and starts again.
#[tokio::test]
#[ignore = "spawned as the MCP stdio gateway by killed_gateway_mid_submit_places_one_order_not_two"]
async fn recovery_demo_gateway() {
    let Ok(root) = std::env::var(ROOT_ENV) else {
        return;
    };
    let runtime = RuntimeService::build(&config(Path::new(&root)))
        .await
        .expect("runtime builds on the shared data directory");
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(PRINCIPAL),
            [
                Capability::SessionRead,
                Capability::SessionWrite,
                Capability::PageRead,
                Capability::PageWrite,
                Capability::BrowserMutate,
                Capability::IntentExecute,
                Capability::RecoveryRead,
                Capability::RecoveryWrite,
            ],
            Utc::now() + Duration::minutes(10),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let server = mcp_gateway::Server::new(Arc::new(AuthenticatedRuntime::new(runtime, handle)));
    server
        .serve(tokio::io::stdin(), tokio::io::stdout())
        .await
        .unwrap();
}

#[derive(Default)]
struct Shop {
    orders: AtomicUsize,
    order_received: Notify,
}

async fn shop_page(State(shop): State<Arc<Shop>>) -> Html<String> {
    Html(format!(
        "<!doctype html><title>Shop</title><main><h1>Checkout</h1>\
         <p>Orders placed: {}</p>\
         <form method=\"post\" action=\"/order\"><button type=\"submit\">Place order</button></form>\
         </main>",
        shop.orders.load(Ordering::SeqCst)
    ))
}

/// Counts the order, then holds the response: the browser is still waiting
/// for the confirmation when the gateway dies.
async fn place_order(State(shop): State<Arc<Shop>>) -> Html<&'static str> {
    shop.orders.fetch_add(1, Ordering::SeqCst);
    shop.order_received.notify_one();
    tokio::time::sleep(StdDuration::from_secs(300)).await;
    Html("<!doctype html><title>Confirmed</title><p>Order confirmed</p>")
}

async fn start_shop() -> (Arc<Shop>, String) {
    let shop = Arc::new(Shop::default());
    let app = Router::new()
        .route("/shop", get(shop_page))
        .route("/order", axum::routing::post(place_order))
        .with_state(Arc::clone(&shop));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (shop, origin)
}

struct Gateway {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

impl Gateway {
    async fn start(root: &Path) -> Self {
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "recovery_demo_gateway",
                "--nocapture",
            ])
            .env(ROOT_ENV, root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut gateway = Self {
            child,
            stdin,
            lines,
            next_id: 0,
        };
        let initialized = gateway
            .request(
                "initialize",
                json!({"protocolVersion":"2025-11-25","capabilities":{},
                       "clientInfo":{"name":"recovery-demo","version":"1"}}),
            )
            .await;
        assert!(initialized.get("result").is_some(), "{initialized}");
        gateway
            .send(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
            .await;
        gateway
    }

    async fn send(&mut self, message: Value) {
        let mut line = serde_json::to_vec(&message).unwrap();
        line.push(b'\n');
        self.stdin.write_all(&line).await.unwrap();
    }

    async fn send_call(&mut self, tool: &str, arguments: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        println!("agent -> {tool} {arguments}");
        self.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                         "params":{"name":tool,"arguments":arguments}}))
            .await;
        id
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await;
        self.response(id).await
    }

    async fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let id = self.send_call(tool, arguments).await;
        self.response(id).await
    }

    async fn response(&mut self, id: u64) -> Value {
        tokio::time::timeout(StdDuration::from_secs(90), async {
            loop {
                let line = self
                    .lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("gateway stdout closed");
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    if value["id"] == id {
                        return value;
                    }
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("gateway did not answer request {id} within 90s"))
    }

    async fn kill(mut self) {
        self.child.kill().await.unwrap();
        let status = self.child.wait().await.unwrap();
        assert!(!status.success(), "gateway was not killed");
    }
}

fn submit_order(workflow_handle: &Value) -> Value {
    json!({
        "workflowHandle": workflow_handle,
        "idempotencyKey": ORDER_KEY,
        "purpose": "Place the order",
        "hints": {"role":"button","accessibleName":"Place order"},
        "expectedState": {
            "condition": {"kind":"url","matcher":{"kind":"contains","value":"/confirmed"}},
            "timeoutMs": 30000
        }
    })
}

async fn open_shop(gateway: &mut Gateway, url: &str) -> Value {
    let started = gateway
        .call("workflow_start", json!({"profile":"shop","url":url}))
        .await;
    let handle = started["result"]["structuredContent"]["workflowHandle"].clone();
    assert!(handle.is_string(), "{started}");
    handle
}

#[tokio::test]
#[ignore = "requires installed Chromium"]
async fn killed_gateway_mid_submit_places_one_order_not_two() {
    let root = tempfile::tempdir().unwrap();
    let (shop, origin) = start_shop().await;
    let shop_url = format!("{origin}/shop");

    let mut first = Gateway::start(root.path()).await;
    let handle = open_shop(&mut first, &shop_url).await;
    let submit = first
        .send_call("intent_submit_and_verify", submit_order(&handle))
        .await;
    tokio::select! {
        () = shop.order_received.notified() => {}
        answered = first.response(submit) => {
            panic!("the submit answered before the order reached the shop: {answered}")
        }
        () = tokio::time::sleep(StdDuration::from_secs(60)) => {
            panic!("the shop never received the order")
        }
    }
    println!("shop    <- POST /order (orders placed: 1); the response is still pending");
    first.kill().await;
    println!("gateway killed mid-submit");

    let mut second = Gateway::start(root.path()).await;
    println!("gateway restarted on the same data directory");
    let handle = open_shop(&mut second, &shop_url).await;
    let retry = second
        .call("intent_submit_and_verify", submit_order(&handle))
        .await;
    let message = retry["error"]["message"].as_str().unwrap_or_default();
    println!("agent <- {message}");
    let refused = &retry["error"]["data"]["interfaceError"];
    assert_eq!(refused["code"], "idempotencyConflict", "{retry}");
    assert_eq!(refused["reconciliationRequired"], true, "{retry}");
    assert!(message.contains("Do not retry"), "{message}");
    assert!(!message.contains("Mint a fresh"), "{message}");

    let observed = second
        .call(
            "workflow_observe",
            json!({"workflowHandle":handle,"goal":"Orders placed"}),
        )
        .await;
    let page = observed["result"].to_string();
    assert!(page.contains("Orders placed: 1"), "{observed}");
    println!("agent <- page reads \"Orders placed: 1\"; nothing to resubmit");

    tokio::time::sleep(StdDuration::from_secs(1)).await;
    assert_eq!(shop.orders.load(Ordering::SeqCst), 1, "one order, not two");

    second.kill().await;
    // Each gateway's Chrome outlives its SIGKILL. The second gateway's factory
    // reaped the first one's; this one reaps the second's.
    let _reaper = ChromiumWorkerFactory::new(config(root.path()).browser);
}
