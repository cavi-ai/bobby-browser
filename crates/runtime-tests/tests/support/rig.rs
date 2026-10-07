//! In-process runtime behind the production MCP server, on either engine,
//! plus the JSON helpers the regression cases read results with.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, ServerConfig, StorageConfig};
use interface_conformance::live::all_capabilities;
use interface_core::AuthorityStore;
use mcp_gateway::Server;
use runtime_tests::{InstalledFirefoxConfig, InstalledFirefoxRuntime};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use types::PrincipalId;

pub struct Rig {
    server: Server,
    next_id: AtomicU64,
    _root: tempfile::TempDir,
    _firefox: Option<InstalledFirefoxRuntime>,
}

fn config(root: &std::path::Path) -> AppConfig {
    AppConfig {
        http: config::HttpConfig {
            allow_loopback: true,
            ..config::HttpConfig::default()
        },
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            shutdown_timeout_ms: 10_000,
        },
        browser: BrowserConfig {
            executable: None,
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
        storage: StorageConfig {
            journal_path: root.join("commands.jsonl"),
            checkpoints_dir: root.join("checkpoints"),
            authority_path: root.join("authority.json"),
            scheduler_journal_path: root.join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
        context: Default::default(),
        nodes: Default::default(),
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
    }
}

impl Rig {
    pub async fn chromium() -> Self {
        let root = tempfile::tempdir().expect("create rig root");
        let mut config = config(root.path());
        if let Some(executable) = std::env::var_os("BOBBY_CHROME_EXECUTABLE") {
            config.browser.executable = Some(executable.into());
        }
        let service = RuntimeService::build(&config)
            .await
            .expect("build Chromium runtime");
        Self::serve(root, service, None).await
    }

    pub async fn firefox() -> Self {
        let installed = InstalledFirefoxConfig::from_env()
            .unwrap_or_else(|name| panic!("{name} must be set to run the Firefox suite"));
        let root = tempfile::tempdir().expect("create rig root");
        let config = config(root.path());
        let firefox =
            runtime_tests::launch_installed_firefox_runtime(installed, &config, "about:blank")
                .await
                .expect("launch installed Firefox runtime");
        let service = RuntimeService::build_with_worker_factory(&config, firefox.factory())
            .await
            .expect("build Firefox runtime");
        Self::serve(root, service, Some(firefox)).await
    }

    async fn serve(
        root: tempfile::TempDir,
        service: RuntimeService,
        firefox: Option<InstalledFirefoxRuntime>,
    ) -> Self {
        let authority = Arc::new(AuthorityStore::in_memory());
        let token = authority
            .issue(
                PrincipalId::from_uuid(uuid::Uuid::new_v4()),
                all_capabilities(),
                Utc::now() + Duration::minutes(30),
            )
            .await
            .expect("issue token")
            .expose_once();
        let handle = authority.verify(&token).await.expect("verify token");
        let server = Server::new(Arc::new(AuthenticatedRuntime::new(service, handle)));
        let rig = Self {
            server,
            next_id: AtomicU64::new(1),
            _root: root,
            _firefox: firefox,
        };
        let initialized = rig
            .request(
                "initialize",
                json!({"protocolVersion":"2025-11-25","capabilities":{},
                       "clientInfo":{"name":"site-regressions","version":"1"}}),
            )
            .await;
        assert!(
            initialized.get("result").is_some(),
            "initialize: {initialized}"
        );
        rig.server
            .handle_message(
                json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
            )
            .await;
        rig
    }

    async fn request(&self, method: &str, params: Value) -> Value {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.server
            .handle_message(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await
            .unwrap_or(Value::Null)
    }

    /// The tool's `structuredContent`.
    pub async fn tool(&self, name: &str, arguments: Value) -> Value {
        let response = self
            .request("tools/call", json!({"name":name,"arguments":arguments}))
            .await;
        assert!(
            response.get("error").is_none(),
            "{name} returned a protocol error: {response}"
        );
        response["result"]["structuredContent"].clone()
    }
}

/// One open session and page.
pub struct Live<'a> {
    rig: &'a Rig,
    pub session_id: Value,
    pub page_id: Value,
    pub started: Value,
}

impl<'a> Live<'a> {
    pub async fn open(rig: &'a Rig, url: &str) -> Live<'a> {
        let started = rig
            .tool(
                "workflow_start",
                json!({"profile":"site-regressions","url":url}),
            )
            .await;
        assert_eq!(started["status"], "completed", "workflow_start: {started}");
        let live = Live {
            rig,
            session_id: started["sessionId"].clone(),
            page_id: started["pageId"].clone(),
            started,
        };
        assert!(
            live.session_id.is_string() && live.page_id.is_string(),
            "workflow_start did not return a session and page: {}",
            live.started
        );
        live
    }

    pub async fn call(&self, tool: &str, extra: Value) -> Value {
        let mut arguments = json!({"sessionId":self.session_id,"pageId":self.page_id});
        for (key, value) in extra.as_object().expect("arguments are an object") {
            arguments[key] = value.clone();
        }
        self.rig.tool(tool, arguments).await
    }

    pub async fn observe(&self, extra: Value) -> Value {
        self.call("workflow_observe", extra).await
    }

    pub async fn snapshot(&self, extra: Value) -> Value {
        self.call("a11y_snapshot", extra).await
    }

    pub async fn close(self) {
        let _ = self
            .rig
            .tool("session_close", json!({"sessionId":self.session_id}))
            .await;
    }
}

/// The first object with this `role` (and `name`, when given).
pub fn find_node<'a>(value: &'a Value, role: &str, name: Option<&str>) -> Option<&'a Value> {
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

pub fn assert_node(result: &Value, role: &str, name: Option<&str>) {
    assert!(
        find_node(result, role, name).is_some(),
        "no {role} node{} in the result: {result}",
        name.map(|name| format!(" named {name:?}"))
            .unwrap_or_default()
    );
}

/// Every `(role, target)` pair on a node that carries a target.
pub fn targets_under<'a>(value: &'a Value, out: &mut Vec<(&'a str, &'a Value)>) {
    match value {
        Value::Object(map) => {
            if let (Some(role), Some(target)) = (
                map.get("role").and_then(Value::as_str),
                map.get("target").filter(|target| target.is_object()),
            ) {
                out.push((role, target));
            }
            map.values().for_each(|child| targets_under(child, out));
        }
        Value::Array(items) => items.iter().for_each(|child| targets_under(child, out)),
        _ => {}
    }
}

pub fn strings_under<'a>(value: &'a Value, key: &str, out: &mut Vec<&'a str>) {
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
