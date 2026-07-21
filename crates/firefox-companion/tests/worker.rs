use std::{
    collections::VecDeque,
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
    BidiEvent, BidiTransport, ExtensionObservation, ExtensionObserver, FirefoxCompanionWorker,
    MAX_TRACKED_PAGES,
};
use serde_json::{json, Value};
use tokio::sync::{broadcast, Mutex};
use types::{
    AttachmentId, ClickCommand, ClosePageCommand, CommandError, CompanionId, ErrorCode, ErrorLayer,
    Evidence, InspectCommand, NavigateCommand, OpenPageCommand, PageId, ProfileId, TypeTextCommand,
    WaitUntil, WorkerId,
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
    events: broadcast::Sender<BidiEvent>,
}

impl FakeBidi {
    fn new(scripted: Vec<Result<Value, CommandError>>) -> Arc<Self> {
        let (events, _) = broadcast::channel(16);
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            scripted: Mutex::new(scripted.into()),
            events,
        })
    }

    async fn calls(&self) -> Vec<BidiCall> {
        self.calls.lock().await.clone()
    }

    fn emit(&self, method: &str, params: Value) {
        self.events
            .send(BidiEvent {
                method: method.into(),
                params,
            })
            .unwrap();
    }
}

#[async_trait]
impl BidiTransport for FakeBidi {
    async fn send(&self, method: &str, params: Value) -> Result<Value, CommandError> {
        let mut calls = self.calls.lock().await;
        calls.push(BidiCall {
            method: method.into(),
            params,
        });
        let call_number = calls.len();
        drop(calls);
        if let Some(response) = self.scripted.lock().await.pop_front() {
            return response;
        }
        if method == "browsingContext.create" {
            return Ok(json!({"context": format!("context-{call_number}")}));
        }
        Ok(json!({}))
    }

    fn subscribe_events(&self) -> Option<broadcast::Receiver<BidiEvent>> {
        Some(self.events.subscribe())
    }

    async fn close(&self) -> Result<(), CommandError> {
        Ok(())
    }
}

struct FakeObserver {
    calls: AtomicUsize,
    observation: ExtensionObservation,
}

impl FakeObserver {
    fn new(observation: ExtensionObservation) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            observation,
        })
    }
}

#[async_trait]
impl ExtensionObserver for FakeObserver {
    async fn observe(
        &self,
        _lease: &AttachmentLease,
        _page_id: &PageId,
        _command: &InspectCommand,
    ) -> Result<ExtensionObservation, CommandError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.observation.clone())
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
            native_input: true,
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
        text: "Observed text".into(),
        html: Some("<main>Observed text</main>".into()),
    }
}

fn worker(bidi: Arc<FakeBidi>, observer: Arc<FakeObserver>) -> FirefoxCompanionWorker {
    FirefoxCompanionWorker::new(
        WorkerId::new(),
        PathBuf::from("/profiles/firefox"),
        lease(),
        bidi,
        observer,
    )
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

#[tokio::test]
async fn open_page_and_navigate_map_to_bidi_context_commands() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"url": "https://example.test/final", "navigation": "nav-1"})),
        Ok(json!({"url": "https://example.test/interactive", "navigation": "nav-2"})),
    ]);
    let worker = worker(bidi.clone(), FakeObserver::new(observation()));
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();

    let complete = worker
        .navigate(
            &page,
            &NavigateCommand {
                url: "https://example.test/final".into(),
                wait_until: WaitUntil::NetworkIdle,
                timeout_ms: 1_000,
            },
        )
        .await
        .unwrap();
    let interactive = worker
        .navigate(
            &page,
            &NavigateCommand {
                url: "https://example.test/interactive".into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 1_000,
            },
        )
        .await
        .unwrap();

    let calls = bidi.calls().await;
    assert_eq!(
        calls[0],
        BidiCall {
            method: "browsingContext.create".into(),
            params: json!({"type": "tab"}),
        }
    );
    assert_eq!(
        calls[1],
        BidiCall {
            method: "browsingContext.navigate".into(),
            params: json!({
                "context": "context-1",
                "url": "https://example.test/final",
                "wait": "complete"
            }),
        }
    );
    assert_eq!(calls[2].params["wait"], "interactive");
    assert_engine_native(&complete);
    assert_engine_native(&interactive);
}

#[tokio::test]
async fn open_page_command_returns_page_and_browser_execution_evidence() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"url": "https://example.test/", "navigation": "nav-1"})),
    ]);
    let worker = worker(bidi, FakeObserver::new(observation()));

    let evidence = worker
        .open_page_command(&OpenPageCommand {
            url: Some("https://example.test/".into()),
        })
        .await
        .unwrap();

    assert!(evidence.iter().any(|item| matches!(
        item,
        Evidence::Page { url, .. } if url == "https://example.test/"
    )));
    assert_engine_native(&evidence);
}

#[tokio::test]
async fn inspect_evaluates_in_isolated_realm_and_uses_extension_observation() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"realm": "realm-1", "result": {"type": "boolean", "value": true}})),
    ]);
    let observer = FakeObserver::new(observation());
    let worker = worker(bidi.clone(), observer.clone());
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();

    let evidence = worker
        .inspect(
            &page,
            &InspectCommand {
                selector: Some("main".into()),
                target: None,
                include_html: true,
            },
        )
        .await
        .unwrap();

    let calls = bidi.calls().await;
    assert_eq!(calls[1].method, "script.evaluate");
    assert_eq!(calls[1].params["target"]["context"], "context-1");
    assert_eq!(
        calls[1].params["target"]["sandbox"],
        "automation-runtime-companion"
    );
    assert_eq!(observer.calls.load(Ordering::SeqCst), 1);
    assert!(evidence.iter().any(|item| matches!(
        item,
        Evidence::Inspection { text, html, .. }
            if text == "Observed text" && html.as_deref() == Some("<main>Observed text</main>")
    )));
}

#[tokio::test]
async fn click_uses_native_pointer_actions_and_engine_native_evidence() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"result": {"type": "node", "sharedId": "element-1"}})),
        Ok(json!({})),
    ]);
    let worker = worker(bidi.clone(), FakeObserver::new(observation()));
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
            },
        )
        .await
        .unwrap();

    let calls = bidi.calls().await;
    assert_eq!(calls[2].method, "input.performActions");
    assert_eq!(calls[2].params["context"], "context-1");
    assert_eq!(calls[2].params["actions"][0]["type"], "pointer");
    assert_eq!(
        calls[2].params["actions"][0]["actions"][0]["origin"]["element"]["sharedId"],
        "element-1"
    );
    assert_engine_native(&evidence);
}

#[tokio::test]
async fn type_text_uses_native_focus_clear_and_key_sequences_without_content_evidence() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"result": {"type": "node", "sharedId": "element-1"}})),
        Ok(json!({})),
        Ok(json!({})),
    ]);
    let worker = worker(bidi.clone(), FakeObserver::new(observation()));
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();

    let evidence = worker
        .type_text(
            &page,
            &TypeTextCommand {
                selector: "input[name=q]".into(),
                target: None,
                value: "Hi".into(),
                clear_first: true,
            },
        )
        .await
        .unwrap();

    let calls = bidi.calls().await;
    assert_eq!(calls[2].method, "input.performActions");
    assert_eq!(calls[2].params["actions"][0]["type"], "pointer");
    assert_eq!(calls[3].method, "input.performActions");
    assert_eq!(calls[3].params["actions"][0]["type"], "key");
    let keys = calls[3].params["actions"][0]["actions"].as_array().unwrap();
    assert!(keys.iter().any(|action| action["value"] == "a"));
    assert!(keys.iter().any(|action| action["value"] == "\u{e003}"));
    assert!(keys.iter().any(|action| action["value"] == "H"));
    assert!(keys.iter().any(|action| action["value"] == "i"));
    assert!(!evidence.iter().any(|item| matches!(
        item,
        Evidence::Element { text: Some(text), .. } if text == "Hi"
    )));
    assert_engine_native(&evidence);
}

#[tokio::test]
async fn missing_page_context_is_not_found_without_transport_calls() {
    let bidi = FakeBidi::new(vec![]);
    let worker = worker(bidi.clone(), FakeObserver::new(observation()));

    let error = worker
        .click(
            &PageId::new(),
            &ClickCommand {
                selector: "button".into(),
                target: None,
                boundary: false,
                expected_url: None,
            },
        )
        .await
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::NotFound);
    assert!(bidi.calls().await.is_empty());
}

#[tokio::test]
async fn native_input_failure_does_not_fall_back_to_dom_click() {
    let native_error = CommandError {
        code: ErrorCode::BrowserCommandFailed,
        message: "native input rejected".into(),
        layer: ErrorLayer::Driver,
        retryable: false,
    };
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"result": {"type": "node", "sharedId": "element-1"}})),
        Err(native_error),
    ]);
    let worker = worker(bidi.clone(), FakeObserver::new(observation()));
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();

    let error = worker
        .click(
            &page,
            &ClickCommand {
                selector: "button".into(),
                target: None,
                boundary: false,
                expected_url: None,
            },
        )
        .await
        .unwrap_err();

    assert_eq!(error.message, "native input rejected");
    let calls = bidi.calls().await;
    assert_eq!(
        calls
            .iter()
            .map(|call| call.method.as_str())
            .collect::<Vec<_>>(),
        vec![
            "browsingContext.create",
            "script.evaluate",
            "input.performActions"
        ]
    );
    assert!(!calls
        .iter()
        .any(|call| call.params.to_string().contains(".click(")));
}

#[tokio::test]
async fn closed_and_destroyed_contexts_are_removed_from_the_page_map() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({})),
        Ok(json!({"context": "context-2"})),
    ]);
    let worker = worker(bidi.clone(), FakeObserver::new(observation()));
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();
    worker
        .close_page_command(&ClosePageCommand {
            page_id: page.clone(),
        })
        .await
        .unwrap();
    worker.open_page(page.clone()).await.unwrap();

    bidi.emit(
        "browsingContext.contextDestroyed",
        json!({"context": "context-2"}),
    );
    tokio::task::yield_now().await;
    let error = worker
        .navigate(
            &page,
            &NavigateCommand {
                url: "https://example.test".into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 100,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn page_context_map_rejects_growth_beyond_its_bound() {
    let bidi = FakeBidi::new(vec![]);
    let worker = worker(bidi.clone(), FakeObserver::new(observation()));
    for _ in 0..MAX_TRACKED_PAGES {
        worker.open_page(PageId::new()).await.unwrap();
    }

    let error = worker.open_page(PageId::new()).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::ResourceExhausted);
    assert_eq!(bidi.calls().await.len(), MAX_TRACKED_PAGES);
}
