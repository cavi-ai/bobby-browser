//! Behavioral BiDi wiring e2e (FakeBidi).
//!
//! Asserts Firefox companion emits human-like `input.performActions` streams
//! for click / type / scroll — the contract we iterate against when hardening
//! the companion bridge.
//!
//! Run:
//! ```text
//! cargo test -p firefox-companion --test behavioral_e2e -- --nocapture
//! make behavioral-e2e
//! ```

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use companion_core::AttachmentLease;
use companion_protocol::{BrowserEngine, BrowserIdentity, CompanionCapabilities, InteractionPath};
use firefox_companion::{
    BidiEvent, BidiTransport, ExtensionControl, ExtensionObservation, ExtensionObserver,
    ExtensionPageBinding, FirefoxCompanionWorker,
};
use serde_json::{json, Value};
use tokio::sync::{broadcast, Mutex};
use types::{
    AttachmentId, ClickCommand, CommandError, CompanionId, Evidence, InspectCommand, PageId,
    ProfileId, TypeTextCommand, WorkerId,
};
use worker_pool::BrowserWorker;

#[derive(Debug, Clone, PartialEq)]
struct BidiCall {
    method: String,
    params: Value,
}

struct FakeBidi {
    calls: Mutex<Vec<BidiCall>>,
    scripted: Mutex<VecDeque<Result<Value, CommandError>>>,
    scroll_needed: Mutex<bool>,
    tree: Mutex<Value>,
    titles: Mutex<HashMap<String, String>>,
    transport_closes: AtomicUsize,
    events: broadcast::Sender<BidiEvent>,
}

impl FakeBidi {
    fn new(scripted: Vec<Result<Value, CommandError>>) -> Arc<Self> {
        let (events, _) = broadcast::channel(2);
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            scripted: Mutex::new(scripted.into()),
            scroll_needed: Mutex::new(false),
            tree: Mutex::new(json!({"contexts": []})),
            titles: Mutex::new(HashMap::new()),
            transport_closes: AtomicUsize::new(0),
            events,
        })
    }

    async fn set_scroll_needed(&self, needed: bool) {
        *self.scroll_needed.lock().await = needed;
    }

    async fn calls(&self) -> Vec<BidiCall> {
        self.calls.lock().await.clone()
    }
}

#[async_trait]
impl BidiTransport for FakeBidi {
    async fn send(&self, method: &str, params: Value) -> Result<Value, CommandError> {
        let mut calls = self.calls.lock().await;
        calls.push(BidiCall {
            method: method.into(),
            params: params.clone(),
        });
        let call_number = calls.len();
        drop(calls);

        if method == "session.subscribe" {
            return Ok(json!({}));
        }
        if method == "script.addPreloadScript" {
            return Ok(json!({"script": format!("preload-{call_number}")}));
        }
        if method == "script.removePreloadScript" {
            return Ok(json!({}));
        }
        if matches!(
            method,
            "emulation.setUserAgentOverride"
                | "emulation.setLocaleOverride"
                | "emulation.setTimezoneOverride"
                | "browsingContext.setViewport"
        ) {
            return Ok(json!({}));
        }
        if method == "script.evaluate" && params["expression"] == "document.title" {
            let context = params["target"]["context"].as_str().unwrap();
            let title = self
                .titles
                .lock()
                .await
                .get(context)
                .cloned()
                .unwrap_or_else(|| "Original tab title".into());
            return Ok(json!({"result": {"type": "string", "value": title}}));
        }
        if method == "script.evaluate"
            && params["expression"]
                .as_str()
                .is_some_and(|expression| expression.contains("document.title="))
        {
            let context = params["target"]["context"].as_str().unwrap();
            let expression = params["expression"].as_str().unwrap();
            if let Some(title) = assigned_title(expression) {
                self.titles.lock().await.insert(context.into(), title);
            }
            return Ok(json!({"result": {"type": "boolean", "value": true}}));
        }
        if method == "browsingContext.getTree" {
            return Ok(self.tree.lock().await.clone());
        }
        if method == "script.evaluate"
            && params["expression"]
                .as_str()
                .is_some_and(|expression| expression.contains("JSON.stringify({valid:"))
        {
            return Ok(
                json!({"result": {"type": "string", "value": "{\"valid\":true,\"message\":\"\"}"}}),
            );
        }
        if method == "script.evaluate"
            && params["expression"]
                .as_str()
                .is_some_and(|expression| expression.contains("automationTypedControlValue"))
        {
            return Ok(json!({
                "result": {
                    "type": "string",
                    "value": "{\"automationTypedControlValue\":\"Hi\"}"
                }
            }));
        }
        if method == "script.callFunction" {
            if params["functionDeclaration"]
                .as_str()
                .is_some_and(|declaration| declaration.contains("automationScrollMetrics"))
            {
                let needed = *self.scroll_needed.lock().await;
                let value = if needed {
                    "{\"needed\":true,\"currentY\":0,\"targetY\":1800,\"viewportHeight\":800,\"pageHeight\":4000}"
                } else {
                    "{\"needed\":false,\"currentY\":0,\"targetY\":0,\"viewportHeight\":800,\"pageHeight\":800}"
                };
                return Ok(json!({
                    "result": {
                        "type": "string",
                        "value": value
                    }
                }));
            }
            if params["functionDeclaration"]
                .as_str()
                .is_some_and(|declaration| declaration.contains("automationPointerBounds"))
            {
                return Ok(json!({
                    "result": {
                        "type": "string",
                        "value": "{\"cx\":960,\"cy\":540,\"width\":1920,\"height\":1080}"
                    }
                }));
            }
            let shared_id = params["arguments"][0]["sharedId"]
                .as_str()
                .unwrap_or("preflight-element");
            return Ok(json!({"result": {"type": "node", "sharedId": shared_id}}));
        }
        if let Some(response) = self.scripted.lock().await.pop_front() {
            if method == "browsingContext.create" {
                if let Ok(response) = &response {
                    if let Some(context) = response.get("context").and_then(Value::as_str) {
                        self.titles
                            .lock()
                            .await
                            .insert(context.into(), "Original tab title".into());
                    }
                }
            }
            return response;
        }
        if method == "browsingContext.create" {
            let context = format!("context-{call_number}");
            self.titles
                .lock()
                .await
                .insert(context.clone(), "Original tab title".into());
            return Ok(json!({"context": context}));
        }
        Ok(json!({}))
    }

    fn subscribe_events(&self) -> Option<broadcast::Receiver<BidiEvent>> {
        Some(self.events.subscribe())
    }

    async fn close(&self) -> Result<(), CommandError> {
        self.transport_closes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn assigned_title(expression: &str) -> Option<String> {
    let encoded = expression
        .split_once("document.title=")?
        .1
        .split_once(";return")?
        .0;
    serde_json::from_str(encoded).ok()
}

struct FakeObserver {
    observation: ExtensionObservation,
}

impl FakeObserver {
    fn new(observation: ExtensionObservation) -> Arc<Self> {
        Arc::new(Self { observation })
    }
}

struct FakePageBinding {
    nonce: String,
}

#[async_trait]
impl ExtensionPageBinding for FakePageBinding {
    fn nonce(&self) -> &str {
        &self.nonce
    }

    async fn complete(self: Box<Self>) -> Result<(), CommandError> {
        Ok(())
    }
}

#[async_trait]
impl ExtensionObserver for FakeObserver {
    async fn begin_page_binding(
        &self,
        _lease: &AttachmentLease,
        _page_id: &PageId,
    ) -> Result<Box<dyn ExtensionPageBinding>, CommandError> {
        Ok(Box::new(FakePageBinding {
            nonce: "b5f6319a-6b36-43cb-9464-d337fc9d8201".into(),
        }))
    }

    async fn observe(
        &self,
        _lease: &AttachmentLease,
        _page_id: &PageId,
        _command: &InspectCommand,
    ) -> Result<ExtensionObservation, CommandError> {
        Ok(self.observation.clone())
    }

    async fn release_page_binding(
        &self,
        _lease: &AttachmentLease,
        _page_id: &PageId,
    ) -> Result<(), CommandError> {
        Ok(())
    }
}

fn lease() -> AttachmentLease {
    AttachmentLease {
        attachment_id: AttachmentId::new(),
        companion_id: CompanionId::new(),
        profile_id: ProfileId::new(),
        identity: BrowserIdentity {
            engine: BrowserEngine::Firefox,
            browser_name: "Firefox".into(),
            browser_version: "128.0".into(),
            os: "test".into(),
            profile_label: "default-release".into(),
        },
        capabilities: CompanionCapabilities {
            observe: true,
            navigate: true,
            native_input: false,
            tabs: true,
            frames: true,
            native_dialogs: false,
        },
        expires_at: Instant::now() + Duration::from_secs(300),
    }
}

fn observation() -> ExtensionObservation {
    ExtensionObservation {
        url: "https://example.test/page".into(),
        title: "Example".into(),
        visible_text: "Observed text".into(),
        controls: vec![ExtensionControl {
            css_path: "#confirm".into(),
            test_id: Some("confirm".into()),
            role: Some("button".into()),
            name: Some("Confirm".into()),
            label: None,
            value: None,
            attributes: BTreeMap::new(),
            disabled: false,
        }],
        html: Some("<main>Observed text</main>".into()),
    }
}

async fn worker(bidi: Arc<FakeBidi>, observer: Arc<FakeObserver>) -> FirefoxCompanionWorker {
    FirefoxCompanionWorker::new(
        WorkerId::new(),
        PathBuf::from("/profiles/firefox"),
        lease(),
        bidi,
        observer,
    )
    .await
    .unwrap()
}

fn assert_engine_native(evidence: &[Evidence]) {
    let expected = serde_json::to_value(InteractionPath::EngineNative)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    assert!(evidence.iter().any(|item| matches!(
        item,
        Evidence::BrowserExecution { interaction_path, .. } if interaction_path == &expected
    )));
}

fn pointer_action_lists(calls: &[BidiCall]) -> Vec<&[Value]> {
    calls
        .iter()
        .filter(|call| {
            call.method == "input.performActions" && call.params["actions"][0]["type"] == "pointer"
        })
        .filter_map(|call| call.params["actions"][0]["actions"].as_array())
        .map(|actions| actions.as_slice())
        .collect()
}

fn key_action_lists(calls: &[BidiCall]) -> Vec<&[Value]> {
    calls
        .iter()
        .filter(|call| {
            call.method == "input.performActions" && call.params["actions"][0]["type"] == "key"
        })
        .filter_map(|call| call.params["actions"][0]["actions"].as_array())
        .map(|actions| actions.as_slice())
        .collect()
}

fn summarize_pointer(actions: &[Value]) -> String {
    let moves = actions
        .iter()
        .filter(|a| a["type"] == "pointerMove")
        .count();
    let pauses = actions.iter().filter(|a| a["type"] == "pause").count();
    let duration: u64 = actions
        .iter()
        .filter(|a| a["type"] == "pointerMove" || a["type"] == "pause")
        .map(|a| a["duration"].as_u64().unwrap_or(0))
        .sum();
    format!("moves={moves} pauses={pauses} duration_ms={duration}")
}

#[tokio::test]
async fn click_emits_curved_pointer_path_with_hover_dwell() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"result": {"type": "node", "sharedId": "element-1"}})),
        Ok(json!({})),
    ]);
    let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();

    let evidence = worker
        .click(
            &page,
            &ClickCommand {
                selector: "button.submit".into(),
                target: None,
                boundary: false,
                expected_url: None,
                modifiers: Vec::new(),
            },
        )
        .await
        .unwrap();

    let calls = bidi.calls().await;
    let pointers = pointer_action_lists(&calls);
    assert_eq!(
        pointers.len(),
        1,
        "expected one pointer performActions for click"
    );
    let actions = pointers[0];
    println!("click pointer stream: {}", summarize_pointer(actions));

    let moves: Vec<&Value> = actions
        .iter()
        .filter(|a| a["type"] == "pointerMove")
        .collect();
    assert!(
        moves.len() >= 4,
        "expected multi-sample approach path (>=4 pointerMoves), got {}",
        moves.len()
    );

    let move_duration: u64 = moves
        .iter()
        .map(|m| m["duration"].as_u64().unwrap_or(0))
        .sum();
    assert!(
        move_duration >= 80,
        "pointerMove duration sum {move_duration}ms too robotic/teleport"
    );

    // At least one positive-duration step after the first sample.
    let positive_steps = moves
        .iter()
        .skip(1)
        .filter(|m| m["duration"].as_u64().unwrap_or(0) > 0)
        .count();
    assert!(
        positive_steps >= 2,
        "expected timed pointer steps, got {positive_steps}"
    );

    let pause_before_down = actions.windows(2).any(|window| {
        window[0]["type"] == "pause"
            && window[0]["duration"].as_u64().unwrap_or(0) > 0
            && window[1]["type"] == "pointerDown"
    });
    assert!(
        pause_before_down,
        "expected hover/session dwell pause before pointerDown"
    );

    assert!(actions.iter().any(|a| a["type"] == "pointerDown"));
    assert!(actions.iter().any(|a| a["type"] == "pointerUp"));
    assert_engine_native(&evidence);
}

#[tokio::test]
async fn type_text_emits_select_all_clear_and_inter_key_pauses() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"result": {"type": "string", "value": "not-select"}})),
        Ok(json!({"result": {"type": "node", "sharedId": "element-1"}})),
        Ok(json!({})),
        Ok(json!({})),
    ]);
    let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();

    worker
        .type_text(
            &page,
            &TypeTextCommand {
                selector: "input[name=q]".into(),
                target: None,
                value: "hello bobby".into(),
                clear_first: true,
                expected_url: None,
            },
        )
        .await
        .unwrap();

    let calls = bidi.calls().await;
    let pointers = pointer_action_lists(&calls);
    assert!(
        !pointers.is_empty(),
        "type_text must focus via pointer actions"
    );
    println!("type focus pointer: {}", summarize_pointer(pointers[0]));

    let keys = key_action_lists(&calls);
    assert_eq!(keys.len(), 1, "expected one key performActions batch");
    let actions = keys[0];

    let pauses = actions
        .iter()
        .filter(|a| a["type"] == "pause" && a["duration"].as_u64().unwrap_or(0) > 0)
        .count();
    assert!(
        pauses >= 3,
        "expected inter-key / clear pauses (>=3), got {pauses}"
    );

    // SelectAll uses modifier + "a".
    let has_select_all = actions.windows(2).any(|window| {
        (window[0]["value"] == "\u{e03d}" || window[0]["value"] == "\u{e009}")
            && window[1]["type"] == "keyDown"
            && window[1]["value"] == "a"
    });
    assert!(has_select_all, "clear_first must emit SelectAll chord");

    assert!(
        actions.iter().any(|a| a["value"] == "\u{e003}"),
        "clear_first must emit Backspace"
    );
    for ch in ["h", "e", "l", "o", " ", "b"] {
        assert!(
            actions
                .iter()
                .any(|a| a["type"] == "keyDown" && a["value"] == ch),
            "missing keyDown for {ch:?}"
        );
    }
}

#[tokio::test]
async fn scroll_into_view_emits_wheel_stream_when_needed() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"result": {"type": "node", "sharedId": "below-fold"}})),
        Ok(json!({})),
    ]);
    bidi.set_scroll_needed(true).await;
    let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();

    worker
        .click(
            &page,
            &ClickCommand {
                selector: "#below-fold".into(),
                target: None,
                boundary: false,
                expected_url: None,
                modifiers: Vec::new(),
            },
        )
        .await
        .unwrap();

    let calls = bidi.calls().await;
    let wheel_batches: Vec<&[Value]> = calls
        .iter()
        .filter(|call| {
            call.method == "input.performActions" && call.params["actions"][0]["type"] == "wheel"
        })
        .filter_map(|call| call.params["actions"][0]["actions"].as_array())
        .map(|actions| actions.as_slice())
        .collect();
    assert!(
        !wheel_batches.is_empty(),
        "scroll-needed click must emit wheel performActions"
    );

    let scroll_steps: usize = wheel_batches
        .iter()
        .map(|batch| batch.iter().filter(|a| a["type"] == "scroll").count())
        .sum();
    assert!(
        scroll_steps >= 1,
        "expected at least one wheel scroll step, got {scroll_steps}"
    );

    let timed = wheel_batches
        .iter()
        .flat_map(|batch| batch.iter())
        .any(|a| a["type"] == "scroll" && a["duration"].as_u64().unwrap_or(0) > 0);
    assert!(timed, "wheel scroll steps must carry duration");

    let pointers = pointer_action_lists(&calls);
    assert_eq!(
        pointers.len(),
        1,
        "click after scroll must still emit one pointer stream"
    );
    assert!(
        pointers[0]
            .iter()
            .filter(|a| a["type"] == "pointerMove")
            .count()
            >= 4,
        "post-scroll click must still use a multi-sample pointer path"
    );
}
