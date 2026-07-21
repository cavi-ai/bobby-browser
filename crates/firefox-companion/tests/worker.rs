use std::{
    collections::{HashMap, VecDeque},
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
    ExtensionPageBinding, FirefoxCompanionWorker, MAX_TRACKED_PAGES,
};
use serde_json::{json, Value};
use tokio::sync::{broadcast, Mutex, Notify};
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
    blocked: Mutex<Option<BlockedSend>>,
    subscribe_error: Mutex<Option<CommandError>>,
    subscribe_response: Mutex<Value>,
    tree: Mutex<Value>,
    titles: Mutex<HashMap<String, String>>,
    closed_titles: Mutex<Vec<String>>,
    events: broadcast::Sender<BidiEvent>,
}

struct BlockedSend {
    method: &'static str,
    expression_contains: Option<&'static str>,
    started: Arc<Notify>,
    release: Arc<Notify>,
}

struct BlockControl {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

impl FakeBidi {
    fn new(scripted: Vec<Result<Value, CommandError>>) -> Arc<Self> {
        let (events, _) = broadcast::channel(2);
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            scripted: Mutex::new(scripted.into()),
            blocked: Mutex::new(None),
            subscribe_error: Mutex::new(None),
            subscribe_response: Mutex::new(json!({})),
            tree: Mutex::new(json!({"contexts": []})),
            titles: Mutex::new(HashMap::new()),
            closed_titles: Mutex::new(Vec::new()),
            events,
        })
    }

    async fn block_once(
        &self,
        method: &'static str,
        expression_contains: Option<&'static str>,
    ) -> BlockControl {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        *self.blocked.lock().await = Some(BlockedSend {
            method,
            expression_contains,
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        });
        BlockControl { started, release }
    }

    async fn fail_subscribe(&self, error: CommandError) {
        *self.subscribe_error.lock().await = Some(error);
    }

    async fn set_subscribe_response(&self, response: Value) {
        *self.subscribe_response.lock().await = response;
    }

    async fn set_tree(&self, tree: Value) {
        *self.tree.lock().await = tree;
    }

    async fn calls(&self) -> Vec<BidiCall> {
        self.calls.lock().await.clone()
    }

    async fn title(&self, context: &str) -> Option<String> {
        self.titles.lock().await.get(context).cloned()
    }

    async fn closed_titles(&self) -> Vec<String> {
        self.closed_titles.lock().await.clone()
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
            params: params.clone(),
        });
        let call_number = calls.len();
        drop(calls);
        let blocked = {
            let mut blocked = self.blocked.lock().await;
            let matches = blocked.as_ref().is_some_and(|blocked| {
                blocked.method == method
                    && blocked.expression_contains.is_none_or(|needle| {
                        params["expression"]
                            .as_str()
                            .is_some_and(|expression| expression.contains(needle))
                    })
            });
            matches.then(|| blocked.take().expect("matching blocked send exists"))
        };
        if let Some(blocked) = blocked {
            blocked.started.notify_one();
            blocked.release.notified().await;
        }
        if method == "session.subscribe" {
            assert!(
                self.events.receiver_count() > 0,
                "event receiver must exist before the remote subscription is enabled"
            );
            if let Some(error) = self.subscribe_error.lock().await.take() {
                return Err(error);
            }
            return Ok(self.subscribe_response.lock().await.clone());
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
        if method == "browsingContext.close" {
            let context = params["context"].as_str().unwrap();
            if let Some(title) = self.titles.lock().await.remove(context) {
                self.closed_titles.lock().await.push(title);
            }
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

fn assigned_title(expression: &str) -> Option<String> {
    let encoded = expression
        .split_once("document.title=")?
        .1
        .split_once(";return")?
        .0;
    serde_json::from_str(encoded).ok()
}

struct FakeObserver {
    calls: AtomicUsize,
    bindings: Mutex<Vec<PageId>>,
    releases: Arc<AtomicUsize>,
    observation: ExtensionObservation,
}

impl FakeObserver {
    fn new(observation: ExtensionObservation) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            bindings: Mutex::new(Vec::new()),
            releases: Arc::new(AtomicUsize::new(0)),
            observation,
        })
    }

    fn with_release_counter(
        observation: ExtensionObservation,
        releases: Arc<AtomicUsize>,
    ) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            bindings: Mutex::new(Vec::new()),
            releases,
            observation,
        })
    }
}

struct FakePageBinding {
    nonce: String,
}

struct BlockingPageBinding {
    nonce: String,
    started: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl ExtensionPageBinding for BlockingPageBinding {
    fn nonce(&self) -> &str {
        &self.nonce
    }

    async fn complete(self: Box<Self>) -> Result<(), CommandError> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

struct BlockingObserver {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

struct BlockingOnceObserver {
    calls: AtomicUsize,
    started: Arc<Notify>,
    release: Arc<Notify>,
    releases: Arc<AtomicUsize>,
}

#[async_trait]
impl ExtensionObserver for BlockingObserver {
    async fn begin_page_binding(
        &self,
        _lease: &AttachmentLease,
        _page_id: &PageId,
    ) -> Result<Box<dyn ExtensionPageBinding>, CommandError> {
        Ok(Box::new(BlockingPageBinding {
            nonce: "b5f6319a-6b36-43cb-9464-d337fc9d8201".into(),
            started: Arc::clone(&self.started),
            release: Arc::clone(&self.release),
        }))
    }

    async fn observe(
        &self,
        _lease: &AttachmentLease,
        _page_id: &PageId,
        _command: &InspectCommand,
    ) -> Result<ExtensionObservation, CommandError> {
        Ok(observation())
    }

    async fn release_page_binding(
        &self,
        _lease: &AttachmentLease,
        _page_id: &PageId,
    ) -> Result<(), CommandError> {
        Ok(())
    }
}

#[async_trait]
impl ExtensionObserver for BlockingOnceObserver {
    async fn begin_page_binding(
        &self,
        _lease: &AttachmentLease,
        _page_id: &PageId,
    ) -> Result<Box<dyn ExtensionPageBinding>, CommandError> {
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        if first {
            Ok(Box::new(BlockingPageBinding {
                nonce: "b5f6319a-6b36-43cb-9464-d337fc9d8201".into(),
                started: Arc::clone(&self.started),
                release: Arc::clone(&self.release),
            }))
        } else {
            Ok(Box::new(FakePageBinding {
                nonce: "b5f6319a-6b36-43cb-9464-d337fc9d8201".into(),
            }))
        }
    }

    async fn observe(
        &self,
        _lease: &AttachmentLease,
        _page_id: &PageId,
        _command: &InspectCommand,
    ) -> Result<ExtensionObservation, CommandError> {
        Ok(observation())
    }

    async fn release_page_binding(
        &self,
        _lease: &AttachmentLease,
        _page_id: &PageId,
    ) -> Result<(), CommandError> {
        self.releases.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
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
        page_id: &PageId,
    ) -> Result<Box<dyn ExtensionPageBinding>, CommandError> {
        self.bindings.lock().await.push(page_id.clone());
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.observation.clone())
    }

    async fn release_page_binding(
        &self,
        _lease: &AttachmentLease,
        _page_id: &PageId,
    ) -> Result<(), CommandError> {
        self.releases.fetch_add(1, Ordering::SeqCst);
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
        visible_text: "Observed text".into(),
        controls: vec![ExtensionControl {
            css_path: "#confirm".into(),
            role: Some("button".into()),
            name: Some("Confirm".into()),
            label: None,
            value: None,
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

#[tokio::test]
async fn worker_subscribes_to_context_destruction_before_exposure_and_propagates_failure() {
    let subscribe_error = CommandError {
        code: ErrorCode::BrowserCommandFailed,
        message: "subscription rejected".into(),
        layer: ErrorLayer::Driver,
        retryable: false,
    };
    let bidi = FakeBidi::new(vec![]);
    bidi.fail_subscribe(subscribe_error.clone()).await;

    let result = FirefoxCompanionWorker::new(
        WorkerId::new(),
        PathBuf::from("/profiles/firefox"),
        lease(),
        bidi.clone(),
        FakeObserver::new(observation()),
    )
    .await;

    let error = match result {
        Ok(_) => panic!("worker must not be exposed after subscription failure"),
        Err(error) => error,
    };
    assert_eq!(error.message, subscribe_error.message);
    assert_eq!(
        bidi.calls().await,
        vec![BidiCall {
            method: "session.subscribe".into(),
            params: json!({"events": ["browsingContext.contextDestroyed"]}),
        }]
    );
}

#[tokio::test]
async fn worker_rejects_non_object_subscribe_results_before_exposure() {
    for response in [Value::Null, json!("not-a-map"), json!([])] {
        let bidi = FakeBidi::new(vec![]);
        bidi.set_subscribe_response(response).await;
        let result = FirefoxCompanionWorker::new(
            WorkerId::new(),
            PathBuf::from("/profiles/firefox"),
            lease(),
            bidi,
            FakeObserver::new(observation()),
        )
        .await;
        let error = match result {
            Ok(_) => panic!("worker must not be exposed after a malformed subscribe result"),
            Err(error) => error,
        };
        assert_eq!(error.code, ErrorCode::BrowserCommandFailed);
        assert_eq!(error.layer, ErrorLayer::Driver);
        assert!(!error.retryable);
    }
}

#[tokio::test]
async fn context_destroyed_while_binding_cannot_be_exposed_as_ready() {
    let bidi = FakeBidi::new(vec![Ok(json!({"context": "context-doomed"}))]);
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let worker = Arc::new(
        FirefoxCompanionWorker::new(
            WorkerId::new(),
            PathBuf::from("/profiles/firefox"),
            lease(),
            bidi.clone(),
            Arc::new(BlockingObserver {
                started: Arc::clone(&started),
                release: Arc::clone(&release),
            }),
        )
        .await
        .unwrap(),
    );
    let page_id = PageId::new();
    let opening_worker = Arc::clone(&worker);
    let opening_page = page_id.clone();
    let opening = tokio::spawn(async move { opening_worker.open_page(opening_page).await });

    started.notified().await;
    bidi.emit(
        "browsingContext.contextDestroyed",
        json!({"context": "context-doomed"}),
    );
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    release.notify_one();

    let error = opening.await.unwrap().unwrap_err();
    assert_eq!(error.code, ErrorCode::BrowserCommandFailed);
    assert!(bidi.calls().await.iter().any(|call| {
        call.method == "browsingContext.close" && call.params["context"] == "context-doomed"
    }));
    assert_eq!(bidi.closed_titles().await, vec!["Original tab title"]);
    assert_eq!(
        worker
            .navigate(
                &page_id,
                &NavigateCommand {
                    url: "https://example.test/should-not-run".into(),
                    wait_until: WaitUntil::Interactive,
                    timeout_ms: 100,
                },
            )
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
}

#[tokio::test]
async fn open_page_uses_a_transient_binding_title_and_restores_the_exact_original() {
    let bidi = FakeBidi::new(vec![Ok(json!({"context": "context-bound"}))]);
    let observer = FakeObserver::new(observation());
    let worker = worker(bidi.clone(), observer.clone()).await;

    let evidence = worker
        .open_page_command(&OpenPageCommand { url: None })
        .await
        .unwrap();

    assert_eq!(observer.bindings.lock().await.len(), 1);
    let calls = bidi.calls().await;
    assert_eq!(calls[0].method, "session.subscribe");
    assert_eq!(calls[1].method, "browsingContext.create");
    assert_eq!(calls[2].method, "script.evaluate");
    assert_eq!(calls[2].params["expression"], "document.title");
    assert_eq!(calls[3].method, "script.evaluate");
    assert_eq!(calls[3].params["target"]["context"], "context-bound");
    assert_eq!(
        calls[3].params["target"]["sandbox"],
        "automation-runtime-companion"
    );
    let marker = calls[3].params["expression"].as_str().unwrap();
    assert!(!marker.contains("data-automation-runtime-binding"));
    assert!(marker.contains("automation-runtime-binding:"));
    assert!(marker.contains("b5f6319a-6b36-43cb-9464-d337fc9d8201"));
    assert_eq!(calls[4].method, "script.evaluate");
    let restore = calls[4].params["expression"].as_str().unwrap();
    assert!(restore.contains("Original tab title"));
    assert!(!restore.contains("automation-runtime-binding:"));
    assert!(evidence.iter().any(|item| matches!(
        item,
        Evidence::Page { title, .. } if title == "Original tab title"
    )));
    assert!(!serde_json::to_string(&evidence)
        .unwrap()
        .contains("automation-runtime-binding:"));
    assert_eq!(
        bidi.title("context-bound").await.as_deref(),
        Some("Original tab title")
    );
}

#[derive(Debug, Clone, Copy)]
enum AbortStage {
    Create,
    BindingMarker,
    Grant,
}

#[tokio::test]
async fn aborting_open_at_each_remote_stage_cleans_context_and_preserves_full_capacity() {
    for stage in [
        AbortStage::Create,
        AbortStage::BindingMarker,
        AbortStage::Grant,
    ] {
        let bidi = FakeBidi::new(vec![]);
        let transport_block = match stage {
            AbortStage::Create => Some(bidi.block_once("browsingContext.create", None).await),
            AbortStage::BindingMarker => Some(
                bidi.block_once("script.evaluate", Some("automation-runtime-binding:"))
                    .await,
            ),
            AbortStage::Grant => None,
        };
        let grant_started = Arc::new(Notify::new());
        let grant_release = Arc::new(Notify::new());
        let release_count = Arc::new(AtomicUsize::new(0));
        let observer: Arc<dyn ExtensionObserver> = match stage {
            AbortStage::Grant => Arc::new(BlockingOnceObserver {
                calls: AtomicUsize::new(0),
                started: Arc::clone(&grant_started),
                release: Arc::clone(&grant_release),
                releases: Arc::clone(&release_count),
            }),
            _ => FakeObserver::with_release_counter(observation(), Arc::clone(&release_count)),
        };
        let worker = Arc::new(
            FirefoxCompanionWorker::new(
                WorkerId::new(),
                PathBuf::from("/profiles/firefox"),
                lease(),
                bidi.clone(),
                observer,
            )
            .await
            .unwrap(),
        );
        let opening_worker = Arc::clone(&worker);
        let opening = tokio::spawn(async move { opening_worker.open_page(PageId::new()).await });

        match stage {
            AbortStage::Grant => grant_started.notified().await,
            _ => {
                transport_block
                    .as_ref()
                    .expect("transport block exists")
                    .started
                    .notified()
                    .await
            }
        }
        opening.abort();
        let _ = opening.await;
        match stage {
            AbortStage::Grant => grant_release.notify_one(),
            _ => transport_block
                .as_ref()
                .expect("transport block exists")
                .release
                .notify_one(),
        }

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if bidi
                    .calls()
                    .await
                    .iter()
                    .any(|call| call.method == "browsingContext.close")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{stage:?} cancellation did not close its context"));
        let closed_titles = bidi.closed_titles().await;
        assert_eq!(closed_titles, vec!["Original tab title"]);
        assert!(!closed_titles
            .iter()
            .any(|title| title.contains("automation-runtime-binding:")));
        assert_eq!(release_count.load(Ordering::SeqCst), 1);

        for _ in 0..MAX_TRACKED_PAGES {
            worker.open_page(PageId::new()).await.unwrap();
        }
        assert_eq!(
            worker.open_page(PageId::new()).await.unwrap_err().code,
            ErrorCode::ResourceExhausted,
            "{stage:?} cancellation consumed one of the 256 page slots"
        );
    }
}

#[tokio::test]
async fn open_page_and_navigate_map_to_bidi_context_commands() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"url": "https://example.test/final", "navigation": "nav-1"})),
        Ok(json!({"url": "https://example.test/interactive", "navigation": "nav-2"})),
    ]);
    let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;
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
        calls[1],
        BidiCall {
            method: "browsingContext.create".into(),
            params: json!({"type": "tab"}),
        }
    );
    assert_eq!(
        calls[5],
        BidiCall {
            method: "browsingContext.navigate".into(),
            params: json!({
                "context": "context-1",
                "url": "https://example.test/final",
                "wait": "complete"
            }),
        }
    );
    assert_eq!(calls[6].params["wait"], "interactive");
    assert_engine_native(&complete);
    assert_engine_native(&interactive);
}

#[tokio::test]
async fn open_page_command_returns_page_and_browser_execution_evidence() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"url": "https://example.test/", "navigation": "nav-1"})),
    ]);
    let worker = worker(bidi, FakeObserver::new(observation())).await;

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
    let worker = worker(bidi.clone(), observer.clone()).await;
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
    assert_eq!(calls[5].method, "script.evaluate");
    assert_eq!(calls[5].params["target"]["context"], "context-1");
    assert_eq!(
        calls[5].params["target"]["sandbox"],
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
            },
        )
        .await
        .unwrap();

    let calls = bidi.calls().await;
    assert_eq!(calls[6].method, "input.performActions");
    assert_eq!(calls[6].params["context"], "context-1");
    assert_eq!(calls[6].params["actions"][0]["type"], "pointer");
    assert_eq!(
        calls[6].params["actions"][0]["actions"][0]["origin"]["element"]["sharedId"],
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
    let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;
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
    assert_eq!(calls[6].method, "input.performActions");
    assert_eq!(calls[6].params["actions"][0]["type"], "pointer");
    assert_eq!(calls[7].method, "input.performActions");
    assert_eq!(calls[7].params["actions"][0]["type"], "key");
    let keys = calls[7].params["actions"][0]["actions"].as_array().unwrap();
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
    let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;

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
    assert_eq!(
        bidi.calls()
            .await
            .iter()
            .map(|call| call.method.as_str())
            .collect::<Vec<_>>(),
        vec!["session.subscribe"]
    );
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
    let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;
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
            "session.subscribe",
            "browsingContext.create",
            "script.evaluate",
            "script.evaluate",
            "script.evaluate",
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
    let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;
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
async fn lagged_context_events_resynchronize_and_prune_missing_contexts() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-stale"})),
        Ok(json!({"context": "context-live"})),
        Ok(json!({"url": "https://example.test/live"})),
    ]);
    let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;
    let stale = PageId::new();
    let live = PageId::new();
    worker.open_page(stale.clone()).await.unwrap();
    worker.open_page(live.clone()).await.unwrap();
    bidi.set_tree(json!({
        "contexts": [{"context": "context-live", "children": []}]
    }))
    .await;

    for index in 0..16 {
        bidi.emit("log.entryAdded", json!({"index": index}));
    }
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let stale_error = worker
        .navigate(
            &stale,
            &NavigateCommand {
                url: "https://example.test/stale".into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 100,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(stale_error.code, ErrorCode::NotFound);
    worker
        .navigate(
            &live,
            &NavigateCommand {
                url: "https://example.test/live".into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 100,
            },
        )
        .await
        .unwrap();
    assert!(bidi
        .calls()
        .await
        .iter()
        .any(|call| call.method == "browsingContext.getTree"));
}

#[tokio::test]
async fn failed_open_navigation_rolls_back_context_and_preserves_primary_error() {
    let primary = CommandError {
        code: ErrorCode::DeadlineExceeded,
        message: "navigation timed out".into(),
        layer: ErrorLayer::Driver,
        retryable: true,
    };
    let cleanup = CommandError {
        code: ErrorCode::BrowserCommandFailed,
        message: "close rejected".into(),
        layer: ErrorLayer::Driver,
        retryable: false,
    };
    let attempts = MAX_TRACKED_PAGES + 8;
    let mut scripted = Vec::with_capacity(attempts * 3 + 1);
    for index in 0..attempts {
        scripted.push(Ok(json!({"context": format!("failed-{index}")})));
        scripted.push(Err(primary.clone()));
        scripted.push(Err(cleanup.clone()));
    }
    scripted.push(Ok(json!({"context": "still-available"})));
    let bidi = FakeBidi::new(scripted);
    let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;

    for _ in 0..attempts {
        let error = worker
            .open_page_command(&OpenPageCommand {
                url: Some("https://example.test/fail".into()),
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, primary.code);
        assert_eq!(error.layer, primary.layer);
        assert_eq!(error.retryable, primary.retryable);
        assert!(error.message.contains("navigation timed out"));
        assert!(error.message.contains("cleanup failed"));
        assert!(error.message.contains("close rejected"));
    }

    worker.open_page(PageId::new()).await.unwrap();
    assert_eq!(
        bidi.calls()
            .await
            .iter()
            .filter(|call| call.method == "browsingContext.close")
            .count(),
        attempts
    );
}

#[tokio::test]
async fn page_context_map_rejects_growth_beyond_its_bound() {
    let bidi = FakeBidi::new(vec![]);
    let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;
    for _ in 0..MAX_TRACKED_PAGES {
        worker.open_page(PageId::new()).await.unwrap();
    }

    let error = worker.open_page(PageId::new()).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::ResourceExhausted);
    let calls = bidi.calls().await;
    assert_eq!(calls[0].method, "session.subscribe");
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.method == "browsingContext.create")
            .count(),
        MAX_TRACKED_PAGES
    );
}
