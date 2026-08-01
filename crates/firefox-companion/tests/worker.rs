use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use artifact_store::ArtifactStore;
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
    AttachmentId, ClickAndWaitForDownloadCommand, ClickAndWaitForPopupCommand, ClickCommand,
    ClosePageCommand, CommandError, CompanionId, ErrorCode, ErrorLayer, Evidence, InspectCommand,
    NavigateCommand, OpenPageCommand, PageId, ProfileId, SessionId, TextMatch, TypeTextCommand,
    UploadFilesCommand, WaitCondition, WaitForCommand, WaitUntil, WorkerId,
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
    preflight: Mutex<VecDeque<Result<Value, CommandError>>>,
    blocked: Mutex<Option<BlockedSend>>,
    subscribe_error: Mutex<Option<CommandError>>,
    subscribe_response: Mutex<Value>,
    tree: Mutex<Value>,
    titles: Mutex<HashMap<String, String>>,
    closed_titles: Mutex<Vec<String>>,
    transport_closes: AtomicUsize,
    events: broadcast::Sender<BidiEvent>,
}

struct BlockedSend {
    method: &'static str,
    expression_contains: Option<&'static str>,
    matches_to_skip: usize,
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
            preflight: Mutex::new(VecDeque::new()),
            blocked: Mutex::new(None),
            subscribe_error: Mutex::new(None),
            subscribe_response: Mutex::new(json!({})),
            tree: Mutex::new(json!({"contexts": []})),
            titles: Mutex::new(HashMap::new()),
            closed_titles: Mutex::new(Vec::new()),
            transport_closes: AtomicUsize::new(0),
            events,
        })
    }

    async fn block_once(
        &self,
        method: &'static str,
        expression_contains: Option<&'static str>,
    ) -> BlockControl {
        self.block_after_matches(method, expression_contains, 0)
            .await
    }

    async fn block_after_matches(
        &self,
        method: &'static str,
        expression_contains: Option<&'static str>,
        matches_to_skip: usize,
    ) -> BlockControl {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        *self.blocked.lock().await = Some(BlockedSend {
            method,
            expression_contains,
            matches_to_skip,
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

    async fn set_preflight(&self, responses: Vec<Result<Value, CommandError>>) {
        *self.preflight.lock().await = responses.into();
    }

    async fn title(&self, context: &str) -> Option<String> {
        self.titles.lock().await.get(context).cloned()
    }

    async fn closed_titles(&self) -> Vec<String> {
        self.closed_titles.lock().await.clone()
    }

    fn transport_close_count(&self) -> usize {
        self.transport_closes.load(Ordering::SeqCst)
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
            if !matches {
                None
            } else if blocked
                .as_ref()
                .expect("matching blocked send exists")
                .matches_to_skip
                > 0
            {
                blocked
                    .as_mut()
                    .expect("matching blocked send exists")
                    .matches_to_skip -= 1;
                None
            } else {
                blocked.take()
            }
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
        if method == "script.evaluate"
            && params["expression"]
                .as_str()
                .is_some_and(|expression| expression.contains("JSON.stringify({valid:"))
        {
            return Ok(
                json!({"result": {"type": "string", "value": "{\"valid\":true,\"message\":\"\"}"}}),
            );
        }
        if method == "script.callFunction" {
            if let Some(response) = self.preflight.lock().await.pop_front() {
                return response;
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
    calls: AtomicUsize,
    bindings: Mutex<Vec<PageId>>,
    releases: Arc<AtomicUsize>,
    release_error: Option<CommandError>,
    observation: ExtensionObservation,
}

impl FakeObserver {
    fn new(observation: ExtensionObservation) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            bindings: Mutex::new(Vec::new()),
            releases: Arc::new(AtomicUsize::new(0)),
            release_error: None,
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
            release_error: None,
            observation,
        })
    }

    fn with_release_error(
        observation: ExtensionObservation,
        releases: Arc<AtomicUsize>,
        release_error: CommandError,
    ) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            bindings: Mutex::new(Vec::new()),
            releases,
            release_error: Some(release_error),
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

struct BlockingCleanupObserver {
    started: Arc<Notify>,
    release: Arc<Notify>,
    releases: Arc<AtomicUsize>,
}

struct HangingFirstReleaseObserver {
    first_page: Mutex<Option<PageId>>,
    attempts: Arc<AtomicUsize>,
    successful_releases: Arc<AtomicUsize>,
    timeout: Duration,
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
impl ExtensionObserver for BlockingCleanupObserver {
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
        Ok(observation())
    }

    async fn release_page_binding(
        &self,
        _lease: &AttachmentLease,
        _page_id: &PageId,
    ) -> Result<(), CommandError> {
        self.started.notify_one();
        self.release.notified().await;
        self.releases.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl ExtensionObserver for HangingFirstReleaseObserver {
    fn operation_timeout(&self) -> Duration {
        self.timeout
    }

    async fn begin_page_binding(
        &self,
        _lease: &AttachmentLease,
        page_id: &PageId,
    ) -> Result<Box<dyn ExtensionPageBinding>, CommandError> {
        let mut first_page = self.first_page.lock().await;
        if first_page.is_none() {
            *first_page = Some(page_id.clone());
        }
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
        Ok(observation())
    }

    async fn release_page_binding(
        &self,
        _lease: &AttachmentLease,
        page_id: &PageId,
    ) -> Result<(), CommandError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.first_page.lock().await.as_ref() == Some(page_id) {
            std::future::pending().await
        } else {
            self.successful_releases.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
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
        match &self.release_error {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
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

async fn wait_for_release_count(releases: &AtomicUsize, expected: usize) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while releases.load(Ordering::SeqCst) < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("expected {expected} page-binding releases"));
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
            params: json!({"events": ["browsingContext.contextCreated", "browsingContext.contextDestroyed", "browsingContext.downloadWillBegin", "browsingContext.downloadEnd", "browsingContext.userPromptOpened", "network.beforeRequestSent", "network.responseCompleted", "network.fetchError"]}),
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
async fn destruction_after_ready_but_before_exposure_fails_open_and_finishes_cleanup() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-destroyed-before-exposure"})),
        Ok(json!({"url": "https://example.test/final"})),
    ]);
    let navigation = bidi.block_once("browsingContext.navigate", None).await;
    let releases = Arc::new(AtomicUsize::new(0));
    let observer = FakeObserver::with_release_counter(observation(), Arc::clone(&releases));
    let worker = Arc::new(worker(bidi.clone(), observer).await);
    let opening_worker = Arc::clone(&worker);
    let opening = tokio::spawn(async move {
        opening_worker
            .open_page_command(&OpenPageCommand {
                url: Some("https://example.test/final".into()),
            })
            .await
    });

    navigation.started.notified().await;
    bidi.emit(
        "browsingContext.contextDestroyed",
        json!({"context": "context-destroyed-before-exposure"}),
    );
    wait_for_release_count(&releases, 1).await;
    navigation.release.notify_one();

    let error = opening
        .await
        .unwrap()
        .expect_err("a page destroyed before exposure commit must never be returned");
    assert_eq!(error.code, ErrorCode::BrowserCommandFailed);
    assert!(bidi.calls().await.iter().any(|call| {
        call.method == "browsingContext.close"
            && call.params["context"] == "context-destroyed-before-exposure"
    }));
    assert_eq!(bidi.closed_titles().await, vec!["Original tab title"]);
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

#[tokio::test]
async fn form_snapshot_deserializes_the_shared_projection_over_bidi() {
    let page_id = PageId::new();
    let encoded = serde_json::to_string(&json!({
        "forms": [],
        "groups": [],
        "controls": [{
            "key": "raw-control-1",
            "formKey": null,
            "groupKey": null,
            "tag": "input",
            "inputType": "password",
            "contentEditable": false,
            "explicitRole": null,
            "accessibleName": "Secret",
            "label": "Secret",
            "description": null,
            "placeholder": null,
            "autocomplete": "current-password",
            "value": null,
            "valuePresent": true,
            "checked": false,
            "fileCount": 0,
            "required": true,
            "readOnly": false,
            "disabled": false,
            "pattern": null,
            "minLength": 8,
            "maxLength": 64,
            "min": null,
            "max": null,
            "step": null,
            "multiple": false,
            "accept": [],
            "willValidate": true,
            "valid": false,
            "validityFlags": ["valueMissing"],
            "validationMessage": null,
            "describedBy": [],
            "options": [],
            "framePath": [],
            "shadowPath": []
        }],
        "truncated": false
    }))
    .unwrap();
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-form-snapshot"})),
        Ok(json!({"result": {"type": "string", "value": encoded}})),
    ]);
    let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;
    worker.open_page(page_id.clone()).await.unwrap();

    let evidence = worker.form_snapshot(&page_id, None).await.unwrap();
    assert!(evidence.iter().any(|item| matches!(
        item,
        Evidence::FormSnapshot { snapshot }
            if snapshot.page_id == page_id
                && snapshot.forms.is_empty()
                && matches!(
                    snapshot.unowned_controls.as_slice(),
                    [control]
                        if control.control_kind == types::FormControlKind::Password
                            && control.state == types::FormControlState::Redacted { present: true }
                            && control.supported_operations
                                == vec![
                                    types::FormControlOperation::SetText,
                                    types::FormControlOperation::Clear,
                                ]
                )
    )));
    let calls = bidi.calls().await;
    let snapshot_call = calls
        .iter()
        .find(|call| {
            call.method == "script.evaluate"
                && call.params["target"]["context"] == "context-form-snapshot"
        })
        .expect("shared form projection evaluated through Firefox BiDi");
    assert_eq!(
        snapshot_call.params["target"]["context"],
        "context-form-snapshot"
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
async fn aborting_open_page_command_during_navigation_cleans_owned_page_and_capacity() {
    let bidi = FakeBidi::new(vec![]);
    let navigation = bidi.block_once("browsingContext.navigate", None).await;
    let releases = Arc::new(AtomicUsize::new(0));
    let observer = FakeObserver::with_release_counter(observation(), Arc::clone(&releases));
    let worker = Arc::new(worker(bidi.clone(), observer).await);
    let opening_worker = Arc::clone(&worker);
    let opening = tokio::spawn(async move {
        opening_worker
            .open_page_command(&OpenPageCommand {
                url: Some("https://example.test/post-open".into()),
            })
            .await
    });

    navigation.started.notified().await;
    opening.abort();
    let _ = opening.await;
    navigation.release.notify_one();

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
    .expect("post-open cancellation must close its context");
    wait_for_release_count(&releases, 1).await;
    assert_eq!(bidi.closed_titles().await, vec!["Original tab title"]);

    for _ in 0..MAX_TRACKED_PAGES {
        worker.open_page(PageId::new()).await.unwrap();
    }
    assert_eq!(
        worker.open_page(PageId::new()).await.unwrap_err().code,
        ErrorCode::ResourceExhausted
    );
}

#[derive(Debug, Clone, Copy)]
enum CleanupAbortStage {
    RestoreTitle,
    CloseContext,
    ReleaseBinding,
}

#[tokio::test]
async fn cancelling_error_rollback_at_each_cleanup_await_restarts_the_same_cleanup() {
    for stage in [
        CleanupAbortStage::RestoreTitle,
        CleanupAbortStage::CloseContext,
        CleanupAbortStage::ReleaseBinding,
    ] {
        let primary = CommandError {
            code: ErrorCode::DeadlineExceeded,
            message: "navigation failed before rollback".into(),
            layer: ErrorLayer::Driver,
            retryable: true,
        };
        let bidi = FakeBidi::new(vec![
            Ok(json!({"context": format!("cleanup-{stage:?}")})),
            Err(primary),
        ]);
        let transport_block = match stage {
            CleanupAbortStage::RestoreTitle => Some(
                bidi.block_after_matches("script.evaluate", Some("Original tab title"), 1)
                    .await,
            ),
            CleanupAbortStage::CloseContext => {
                Some(bidi.block_once("browsingContext.close", None).await)
            }
            CleanupAbortStage::ReleaseBinding => None,
        };
        let release_started = Arc::new(Notify::new());
        let release_gate = Arc::new(Notify::new());
        let releases = Arc::new(AtomicUsize::new(0));
        let observer: Arc<dyn ExtensionObserver> = match stage {
            CleanupAbortStage::ReleaseBinding => Arc::new(BlockingCleanupObserver {
                started: Arc::clone(&release_started),
                release: Arc::clone(&release_gate),
                releases: Arc::clone(&releases),
            }),
            _ => FakeObserver::with_release_counter(observation(), Arc::clone(&releases)),
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
        let rollback = tokio::spawn(async move {
            opening_worker
                .open_page_command(&OpenPageCommand {
                    url: Some("https://example.test/fail-before-cleanup".into()),
                })
                .await
        });

        match stage {
            CleanupAbortStage::ReleaseBinding => release_started.notified().await,
            _ => {
                transport_block
                    .as_ref()
                    .expect("transport cleanup block exists")
                    .started
                    .notified()
                    .await
            }
        }
        rollback.abort();
        let _ = rollback.await;
        match stage {
            CleanupAbortStage::ReleaseBinding => release_gate.notify_one(),
            _ => transport_block
                .as_ref()
                .expect("transport cleanup block exists")
                .release
                .notify_one(),
        }

        tokio::time::timeout(Duration::from_millis(100), async {
            while releases.load(Ordering::SeqCst) != 1
                || bidi.closed_titles().await != vec!["Original tab title"]
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{stage:?} cancellation abandoned rollback ownership"));
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
async fn inspect_reads_only_bounded_inert_json_script_receipts() {
    let receipt = r#"{"manifestDigest":"abc","stations":[]}"#;
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"result": {"type": "boolean", "value": true}})),
        Ok(json!({"result": {"type": "string", "value": receipt}})),
    ]);
    let observer = FakeObserver::new(observation());
    let worker = worker(bidi, observer.clone()).await;
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();

    let evidence = worker
        .inspect(
            &page,
            &InspectCommand {
                selector: Some("script[data-testid=station-scorecard]".into()),
                target: None,
                include_html: false,
            },
        )
        .await
        .unwrap();

    assert!(evidence
        .iter()
        .any(|item| matches!(item, Evidence::Inspection { text, .. } if text == receipt)));
    assert_eq!(observer.calls.load(Ordering::SeqCst), 0);
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
    assert_eq!(calls[6].method, "script.callFunction");
    assert_eq!(calls[7].method, "input.performActions");
    assert_eq!(calls[7].params["context"], "context-1");
    assert_eq!(calls[7].params["actions"][0]["type"], "pointer");
    assert_eq!(
        calls[7].params["actions"][0]["actions"][0]["origin"]["element"]["sharedId"],
        "element-1"
    );
    assert_engine_native(&evidence);
}

#[tokio::test]
async fn native_click_scrolls_and_revalidates_a_below_fold_element_before_pointer_input() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"result": {"type": "node", "sharedId": "below-fold"}})),
        Ok(json!({})),
    ]);
    bidi.set_preflight(vec![Ok(json!({"result": {
        "type": "node",
        "sharedId": "below-fold-after-scroll"
    }}))])
    .await;
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
            },
        )
        .await
        .unwrap();

    let calls = bidi.calls().await;
    let preflight = calls
        .iter()
        .find(|call| call.method == "script.callFunction")
        .expect("native click must preflight the live element");
    assert_eq!(preflight.params["arguments"][0]["sharedId"], "below-fold");
    assert!(preflight.params["functionDeclaration"]
        .as_str()
        .unwrap()
        .contains("scrollIntoView"));
    let pointer = calls
        .iter()
        .find(|call| call.method == "input.performActions")
        .unwrap();
    assert_eq!(
        pointer.params["actions"][0]["actions"][0]["origin"]["element"]["sharedId"],
        "below-fold-after-scroll"
    );
}

#[tokio::test]
async fn native_click_fails_typed_without_pointer_input_when_target_detaches_after_scroll() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"result": {"type": "node", "sharedId": "detaching"}})),
    ]);
    bidi.set_preflight(vec![Ok(json!({"result": {
        "type": "string",
        "value": "detached"
    }}))])
    .await;
    let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();

    let error = worker
        .click(
            &page,
            &ClickCommand {
                selector: "#detaching".into(),
                target: None,
                boundary: false,
                expected_url: None,
            },
        )
        .await
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::TargetDetached);
    assert!(!bidi
        .calls()
        .await
        .iter()
        .any(|call| call.method == "input.performActions"));
}

#[tokio::test]
async fn native_click_fails_typed_for_obscured_and_out_of_bounds_targets() {
    for (status, expected) in [
        ("obscured", ErrorCode::TargetObscured),
        ("out-of-bounds", ErrorCode::TargetOutOfBounds),
    ] {
        let bidi = FakeBidi::new(vec![
            Ok(json!({"context": "context-1"})),
            Ok(json!({"result": {"type": "node", "sharedId": "blocked"}})),
        ]);
        bidi.set_preflight(vec![Ok(json!({"result": {
            "type": "string",
            "value": status
        }}))])
        .await;
        let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;
        let page = PageId::new();
        worker.open_page(page.clone()).await.unwrap();

        let error = worker
            .click(
                &page,
                &ClickCommand {
                    selector: "#blocked".into(),
                    target: None,
                    boundary: false,
                    expected_url: None,
                },
            )
            .await
            .unwrap_err();

        assert_eq!(error.code, expected, "preflight status: {status}");
        assert!(!bidi
            .calls()
            .await
            .iter()
            .any(|call| call.method == "input.performActions"));
    }
}

#[tokio::test]
async fn semantic_click_resolves_test_id_to_verified_css_before_native_input() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"result": {"type": "node", "sharedId": "element-1"}})),
        Ok(json!({})),
    ]);
    let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();

    worker
        .click(
            &page,
            &ClickCommand {
                selector: String::new(),
                target: Some(types::TargetSpec {
                    test_id: Some("confirm".into()),
                    ..Default::default()
                }),
                boundary: false,
                expected_url: None,
            },
        )
        .await
        .unwrap();

    let calls = bidi.calls().await;
    assert!(calls[5].params["expression"]
        .as_str()
        .unwrap()
        .contains("[data-testid=\\\"confirm\\\"]"));
    assert_eq!(calls[6].method, "script.callFunction");
    assert_eq!(calls[7].method, "input.performActions");
}

#[tokio::test]
async fn semantic_click_missing_live_node_is_typed_as_target_drift_input() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"result": {"type": "null"}})),
    ]);
    let worker = worker(bidi, FakeObserver::new(observation())).await;
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();

    let error = worker
        .click(
            &page,
            &ClickCommand {
                selector: String::new(),
                target: Some(types::TargetSpec {
                    test_id: Some("initial-target".into()),
                    ..Default::default()
                }),
                boundary: false,
                expected_url: None,
            },
        )
        .await
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::TargetNotFound);
}

#[tokio::test]
async fn semantic_click_descends_exact_test_id_frame_before_native_input() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"result": {"type": "string", "value": "index:0"}})),
        Ok(json!({"result": {"type": "node", "sharedId": "frame-button"}})),
        Ok(json!({})),
    ]);
    bidi.set_tree(json!({"contexts": [{
        "context": "context-1",
        "children": [{"context": "frame-context", "children": []}]
    }]}))
    .await;
    bidi.set_preflight(vec![Ok(json!({"result": {
        "type": "node",
        "sharedId": "frame-button-after-scroll"
    }}))])
    .await;
    let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();

    worker
        .click(
            &page,
            &ClickCommand {
                selector: String::new(),
                target: Some(types::TargetSpec {
                    test_id: Some("iframe-submit".into()),
                    frame_path: vec![Box::new(types::TargetSpec {
                        test_id: Some("iframe-challenge".into()),
                        ..Default::default()
                    })],
                    ..Default::default()
                }),
                boundary: false,
                expected_url: None,
            },
        )
        .await
        .unwrap();

    let calls = bidi.calls().await;
    assert!(calls
        .iter()
        .any(|call| call.method == "browsingContext.getTree"));
    let preflight = calls
        .iter()
        .find(|call| call.method == "script.callFunction")
        .expect("frame click must preflight in the child context");
    assert_eq!(preflight.params["target"]["context"], "frame-context");
    let click = calls
        .iter()
        .find(|call| call.method == "input.performActions")
        .unwrap();
    assert_eq!(click.params["context"], "frame-context");
    assert_eq!(
        click.params["actions"][0]["actions"][0]["origin"]["element"]["sharedId"],
        "frame-button-after-scroll"
    );
}

#[tokio::test]
async fn semantic_frame_path_rejects_missing_ambiguous_and_non_frame_segments() {
    for (probe, expected) in [
        ("missing", ErrorCode::FrameNotFound),
        ("ambiguous", ErrorCode::TargetAmbiguous),
        ("non-frame", ErrorCode::FrameNotFound),
    ] {
        let bidi = FakeBidi::new(vec![
            Ok(json!({"context": "context-1"})),
            Ok(json!({"result": {"type": "string", "value": probe}})),
        ]);
        let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;
        let page = PageId::new();
        worker.open_page(page.clone()).await.unwrap();

        let error = worker
            .click(
                &page,
                &ClickCommand {
                    selector: String::new(),
                    target: Some(types::TargetSpec {
                        test_id: Some("iframe-submit".into()),
                        frame_path: vec![Box::new(types::TargetSpec {
                            test_id: Some("iframe-challenge".into()),
                            ..Default::default()
                        })],
                        ..Default::default()
                    }),
                    boundary: false,
                    expected_url: None,
                },
            )
            .await
            .unwrap_err();

        assert_eq!(error.code, expected, "frame probe result: {probe}");
        assert!(!bidi
            .calls()
            .await
            .iter()
            .any(|call| call.method == "input.performActions"));
    }
}

#[tokio::test]
async fn semantic_click_descends_exact_open_shadow_root_before_native_input() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"result": {"type": "node", "sharedId": "shadow-button"}})),
        Ok(json!({})),
    ]);
    bidi.set_preflight(vec![Ok(json!({"result": {
        "type": "node",
        "sharedId": "shadow-button-after-scroll"
    }}))])
    .await;
    let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();

    worker
        .click(
            &page,
            &ClickCommand {
                selector: String::new(),
                target: Some(types::TargetSpec {
                    test_id: Some("shadow-submit".into()),
                    shadow_path: vec![Box::new(types::TargetSpec {
                        test_id: Some("shadow-host".into()),
                        ..Default::default()
                    })],
                    ..Default::default()
                }),
                boundary: false,
                expected_url: None,
            },
        )
        .await
        .unwrap();

    let calls = bidi.calls().await;
    let probe = calls
        .iter()
        .find(|call| {
            call.method == "script.evaluate"
                && call.params["expression"]
                    .as_str()
                    .is_some_and(|expression| expression.contains("shadowRoot"))
        })
        .unwrap();
    let expression = probe.params["expression"].as_str().unwrap();
    assert!(expression.contains("shadow-host"));
    assert!(expression.contains("shadow-submit"));
    let click = calls
        .iter()
        .find(|call| call.method == "input.performActions")
        .unwrap();
    assert_eq!(
        click.params["actions"][0]["actions"][0]["origin"]["element"]["sharedId"],
        "shadow-button-after-scroll"
    );
}

#[tokio::test]
async fn semantic_shadow_path_rejects_missing_closed_and_ambiguous_roots() {
    for (probe, expected) in [
        ("host-missing", ErrorCode::ShadowRootUnavailable),
        ("shadow-unavailable", ErrorCode::ShadowRootUnavailable),
        ("host-ambiguous", ErrorCode::TargetAmbiguous),
        ("target-missing", ErrorCode::TargetNotFound),
        ("target-ambiguous", ErrorCode::TargetAmbiguous),
    ] {
        let bidi = FakeBidi::new(vec![
            Ok(json!({"context": "context-1"})),
            Ok(json!({"result": {"type": "string", "value": probe}})),
        ]);
        let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;
        let page = PageId::new();
        worker.open_page(page.clone()).await.unwrap();
        let error = worker
            .click(
                &page,
                &ClickCommand {
                    selector: String::new(),
                    target: Some(types::TargetSpec {
                        test_id: Some("shadow-submit".into()),
                        shadow_path: vec![Box::new(types::TargetSpec {
                            test_id: Some("shadow-host".into()),
                            ..Default::default()
                        })],
                        ..Default::default()
                    }),
                    boundary: false,
                    expected_url: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, expected, "shadow probe result: {probe}");
        assert!(!bidi
            .calls()
            .await
            .iter()
            .any(|call| call.method == "input.performActions"));
    }
}

#[tokio::test]
async fn popup_subscription_precedes_one_native_click_and_registers_the_new_page() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"result": {"type": "node", "sharedId": "popup-button"}})),
        Ok(json!({})),
    ]);
    let worker = Arc::new(worker(bidi.clone(), FakeObserver::new(observation())).await);
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();
    let operation = {
        let worker = Arc::clone(&worker);
        let page = page.clone();
        tokio::spawn(async move {
            worker
                .click_and_wait_for_popup(
                    &page,
                    &ClickAndWaitForPopupCommand {
                        selector: String::new(),
                        target: Some(types::TargetSpec {
                            test_id: Some("popup-open".into()),
                            ..Default::default()
                        }),
                        timeout_ms: 1_000,
                    },
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if bidi
                .calls()
                .await
                .iter()
                .any(|call| call.method == "input.performActions")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    bidi.emit(
        "browsingContext.contextCreated",
        json!({
            "context": "popup-context",
            "url": "https://example.test/popup",
            "originalOpener": "context-1",
            "parent": null
        }),
    );
    let evidence = operation.await.unwrap().unwrap();
    let popup_page = evidence
        .iter()
        .find_map(|item| match item {
            Evidence::Popup { page_id, .. } => Some(page_id.clone()),
            _ => None,
        })
        .unwrap();
    worker
        .inspect(&popup_page, &InspectCommand::default())
        .await
        .unwrap();
    assert_eq!(
        bidi.calls()
            .await
            .iter()
            .filter(|call| call.method == "input.performActions")
            .count(),
        1
    );
}

#[tokio::test]
async fn popup_timeout_never_replays_the_boundary_click() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"result": {"type": "node", "sharedId": "popup-button"}})),
        Ok(json!({})),
    ]);
    let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();
    let error = worker
        .click_and_wait_for_popup(
            &page,
            &ClickAndWaitForPopupCommand {
                selector: String::new(),
                target: Some(types::TargetSpec {
                    test_id: Some("popup-open".into()),
                    ..Default::default()
                }),
                timeout_ms: 1,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::WaitConditionTimedOut);
    assert_eq!(
        bidi.calls()
            .await
            .iter()
            .filter(|call| call.method == "input.performActions")
            .count(),
        1
    );
}

#[tokio::test]
async fn popup_capture_ignores_unrelated_contexts_and_handles_event_during_click() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"result": {"type": "node", "sharedId": "popup-button"}})),
        Ok(json!({})),
    ]);
    let click = bidi.block_once("input.performActions", None).await;
    let worker = Arc::new(worker(bidi.clone(), FakeObserver::new(observation())).await);
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();
    let operation = {
        let worker = Arc::clone(&worker);
        let page = page.clone();
        tokio::spawn(async move {
            worker
                .click_and_wait_for_popup(
                    &page,
                    &ClickAndWaitForPopupCommand {
                        selector: String::new(),
                        target: Some(types::TargetSpec {
                            test_id: Some("popup-open".into()),
                            ..Default::default()
                        }),
                        timeout_ms: 1_000,
                    },
                )
                .await
        })
    };
    click.started.notified().await;
    bidi.emit(
        "browsingContext.contextCreated",
        json!({"context": "unrelated", "originalOpener": "other"}),
    );
    bidi.emit(
        "browsingContext.contextCreated",
        json!({
            "context": "popup-context",
            "url": "https://example.test/popup",
            "originalOpener": "context-1"
        }),
    );
    click.release.notify_one();
    assert!(operation.await.unwrap().is_ok());
}

#[tokio::test]
async fn upload_uses_bidi_set_files_and_returns_only_opaque_evidence() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("fixture.txt");
    std::fs::write(&file, b"approved fixture").unwrap();
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"result": {"type": "string", "value": "valid"}})),
        Ok(json!({"result": {"type": "node", "sharedId": "file-input"}})),
        Ok(json!({})),
        Ok(json!({"result": {"type": "number", "value": 1}})),
    ]);
    let worker = worker(bidi.clone(), FakeObserver::new(observation()))
        .await
        .with_upload_roots(vec![root.path().to_path_buf()]);
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();

    let evidence = worker
        .upload_files(
            &page,
            &UploadFilesCommand {
                selector: "input[type=file]".into(),
                target: None,
                paths: vec![file.to_string_lossy().into_owned()],
            },
        )
        .await
        .unwrap();

    let calls = bidi.calls().await;
    let set_files = calls
        .iter()
        .find(|call| call.method == "input.setFiles")
        .unwrap();
    assert_eq!(set_files.params["element"]["sharedId"], "file-input");
    let serialized = serde_json::to_string(&evidence).unwrap();
    assert!(!serialized.contains(root.path().to_string_lossy().as_ref()));
    assert!(serialized.contains("upload://sha256/"));
}

#[tokio::test]
async fn download_correlates_bidi_events_after_one_click_and_returns_artifact_ref() {
    let root = tempfile::tempdir().unwrap();
    let artifacts = ArtifactStore::new(root.path().join("artifacts"), 1024 * 1024, 4096);
    let downloads = root.path().join("downloads");
    let session = SessionId::new();
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({})),
        Ok(json!({"result": {"type": "node", "sharedId": "download-link"}})),
        Ok(json!({})),
        Ok(json!({"result": {"type": "string", "value": "valid"}})),
        Ok(json!({"result": {"type": "node", "sharedId": "file-input"}})),
        Ok(json!({})),
        Ok(json!({"result": {"type": "number", "value": 1}})),
    ]);
    let worker = Arc::new(
        worker(bidi.clone(), FakeObserver::new(observation()))
            .await
            .with_runtime_storage(session.clone(), artifacts, downloads.clone()),
    );
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();
    let operation = {
        let worker = Arc::clone(&worker);
        let page = page.clone();
        tokio::spawn(async move {
            worker
                .click_and_wait_for_download(
                    &page,
                    &ClickAndWaitForDownloadCommand {
                        selector: "a[download]".into(),
                        target: None,
                        timeout_ms: 1_000,
                    },
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if bidi
                .calls()
                .await
                .iter()
                .any(|call| call.method == "input.performActions")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let destination = downloads.join(session.0.to_string());
    std::fs::write(destination.join("receipt.txt"), b"download fixture").unwrap();
    bidi.emit(
        "browsingContext.downloadWillBegin",
        json!({
            "context": "context-1", "navigation": null, "suggestedFilename": "receipt.txt"
        }),
    );
    bidi.emit(
        "browsingContext.downloadEnd",
        json!({
            "context": "context-1", "navigation": null, "status": "complete"
        }),
    );
    let evidence = operation.await.unwrap().unwrap();
    let serialized = serde_json::to_string(&evidence).unwrap();
    assert!(serialized.contains("artifact://"));
    assert!(!serialized.contains(downloads.to_string_lossy().as_ref()));
    let artifact_ref = evidence
        .iter()
        .find_map(|item| match item {
            Evidence::Download { path, .. } => Some(path.clone()),
            _ => None,
        })
        .unwrap();
    let upload = worker
        .upload_files(
            &page,
            &UploadFilesCommand {
                selector: "input[type=file]".into(),
                target: None,
                paths: vec![artifact_ref],
            },
        )
        .await
        .unwrap();
    assert!(!serde_json::to_string(&upload)
        .unwrap()
        .contains(downloads.to_string_lossy().as_ref()));
    assert_eq!(
        bidi.calls()
            .await
            .iter()
            .filter(|call| call.method == "input.performActions")
            .count(),
        1
    );
}

#[tokio::test]
async fn semantic_type_text_resolves_exact_label_before_native_input() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"result": {"type": "string", "value": "not-select"}})),
        Ok(json!({"result": {"type": "node", "sharedId": "element-1"}})),
        Ok(json!({})),
        Ok(json!({})),
    ]);
    let mut observed = observation();
    observed.controls[0].label = Some("Full name".into());
    observed.controls[0]
        .attributes
        .insert("required".into(), "true".into());
    let worker = worker(bidi.clone(), FakeObserver::new(observed)).await;
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();

    worker
        .type_text(
            &page,
            &TypeTextCommand {
                expected_url: None,
                selector: String::new(),
                target: Some(types::TargetSpec {
                    label: Some("Full name".into()),
                    attributes: BTreeMap::from([("required".into(), "true".into())]),
                    ..Default::default()
                }),
                value: "Ada".into(),
                clear_first: true,
            },
        )
        .await
        .unwrap();

    let calls = bidi.calls().await;
    assert!(calls[5].params["expression"]
        .as_str()
        .unwrap()
        .contains("#confirm"));
    assert_eq!(calls[7].method, "script.callFunction");
    assert_eq!(calls[8].method, "input.performActions");
}

#[tokio::test]
async fn semantic_inspect_resolves_label_and_returns_sanitized_control_value() {
    let bidi = FakeBidi::new(vec![Ok(json!({"context": "context-1"}))]);
    let mut observed = observation();
    observed.visible_text.clear();
    observed.controls[0].label = Some("Full name".into());
    observed.controls[0].value = Some("Ada Lovelace".into());
    observed.controls[0]
        .attributes
        .insert("required".into(), "true".into());
    let observer = FakeObserver::new(observed);
    let worker = worker(bidi, observer.clone()).await;
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();

    let evidence = worker
        .inspect(
            &page,
            &InspectCommand {
                selector: None,
                target: Some(types::TargetSpec {
                    label: Some("Full name".into()),
                    attributes: BTreeMap::from([("required".into(), "true".into())]),
                    ..Default::default()
                }),
                include_html: false,
            },
        )
        .await
        .unwrap();

    assert!(evidence
        .iter()
        .any(|item| matches!(item, Evidence::Inspection { text, .. } if text == "Ada Lovelace")));
    assert_eq!(observer.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn raw_css_select_inspect_prefers_machine_value_over_visible_option_label() {
    let bidi = FakeBidi::new(vec![Ok(json!({"context": "context-1"}))]);
    let mut observed = observation();
    observed.visible_text = "Pro".into();
    observed.controls[0].role = Some("combobox".into());
    observed.controls[0].name = Some("Plan".into());
    observed.controls[0].value = Some("pro".into());
    let worker = worker(bidi, FakeObserver::new(observed)).await;
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();

    let evidence = worker
        .inspect(
            &page,
            &InspectCommand {
                selector: Some("select[aria-label='Plan']".into()),
                target: None,
                include_html: false,
            },
        )
        .await
        .unwrap();

    assert!(evidence
        .iter()
        .any(|item| matches!(item, Evidence::Inspection { text, .. } if text == "pro")));
}

#[tokio::test]
async fn wait_for_url_uses_exact_bounded_matcher_semantics() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(
            json!({"result": {"type": "string", "value": "https://example.test/page?checkpoint=7"}}),
        ),
    ]);
    let worker = worker(bidi, FakeObserver::new(observation())).await;
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();

    let evidence = worker
        .wait_for(
            &page,
            &WaitForCommand {
                condition: WaitCondition::Url {
                    matcher: TextMatch::Contains("/page".into()),
                },
                timeout_ms: 100,
            },
        )
        .await
        .unwrap();

    assert!(matches!(
        evidence.as_slice(),
        [Evidence::Wait {
            observations: 1,
            ..
        }]
    ));
}

#[tokio::test]
async fn type_text_uses_native_focus_clear_and_key_sequences_without_content_evidence() {
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

    let evidence = worker
        .type_text(
            &page,
            &TypeTextCommand {
                selector: "input[name=q]".into(),
                target: None,
                value: "Hi".into(),
                clear_first: true,
                expected_url: None,
            },
        )
        .await
        .unwrap();

    let calls = bidi.calls().await;
    assert_eq!(calls[7].method, "script.callFunction");
    assert_eq!(calls[8].method, "input.performActions");
    assert_eq!(calls[8].params["actions"][0]["type"], "pointer");
    assert_eq!(calls[9].method, "input.performActions");
    assert_eq!(calls[9].params["actions"][0]["type"], "key");
    let keys = calls[9].params["actions"][0]["actions"].as_array().unwrap();
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
async fn type_text_selects_an_exact_option_value_without_keyboard_input() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-1"})),
        Ok(json!({"result": {"type": "string", "value": "selected"}})),
    ]);
    let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();

    let evidence = worker
        .type_text(
            &page,
            &TypeTextCommand {
                selector: "select[aria-label='Plan']".into(),
                target: None,
                value: "pro".into(),
                clear_first: true,
                expected_url: None,
            },
        )
        .await
        .unwrap();

    let calls = bidi.calls().await;
    assert!(calls.iter().any(|call| {
        call.method == "script.evaluate"
            && call.params["expression"]
                .as_str()
                .is_some_and(|expression| expression.contains("HTMLSelectElement"))
    }));
    assert!(!calls
        .iter()
        .any(|call| call.method == "input.performActions"));
    let expected = serde_json::to_value(InteractionPath::ExtensionApi)
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
async fn type_text_select_reports_missing_disabled_and_ambiguous_options() {
    for (result, expected_code) in [
        ("missing", ErrorCode::TargetNotFound),
        ("disabled", ErrorCode::TargetNotFound),
        ("ambiguous", ErrorCode::TargetAmbiguous),
    ] {
        let bidi = FakeBidi::new(vec![
            Ok(json!({"context": "context-1"})),
            Ok(json!({"result": {"type": "string", "value": result}})),
        ]);
        let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;
        let page = PageId::new();
        worker.open_page(page.clone()).await.unwrap();

        let error = worker
            .type_text(
                &page,
                &TypeTextCommand {
                    selector: "select[aria-label='Plan']".into(),
                    target: None,
                    value: "pro".into(),
                    clear_first: true,
                    expected_url: None,
                },
            )
            .await
            .unwrap_err();

        assert_eq!(error.code, expected_code, "select probe result: {result}");
        assert!(!bidi
            .calls()
            .await
            .iter()
            .any(|call| call.method == "input.performActions"));
    }
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
            "script.callFunction",
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
    let observer = FakeObserver::new(observation());
    let releases = Arc::clone(&observer.releases);
    let worker = worker(bidi.clone(), observer).await;
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
    wait_for_release_count(&releases, 2).await;
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
async fn worker_close_cleans_ready_pages_once_and_prevents_reopening() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-close-one"})),
        Ok(json!({"context": "context-close-two"})),
    ]);
    let releases = Arc::new(AtomicUsize::new(0));
    let observer = FakeObserver::with_release_counter(observation(), Arc::clone(&releases));
    let worker = worker(bidi.clone(), observer).await;
    worker.open_page(PageId::new()).await.unwrap();
    worker.open_page(PageId::new()).await.unwrap();

    worker.close().await.unwrap();
    assert_eq!(
        bidi.calls()
            .await
            .iter()
            .filter(|call| call.method == "browsingContext.close")
            .count(),
        2
    );
    assert_eq!(releases.load(Ordering::SeqCst), 2);
    let error = worker
        .open_page(PageId::new())
        .await
        .expect_err("a closed worker must not create another context");
    assert_eq!(error.code, ErrorCode::BrowserCommandFailed);

    worker.close().await.unwrap();
    assert_eq!(
        bidi.calls()
            .await
            .iter()
            .filter(|call| call.method == "browsingContext.close")
            .count(),
        2
    );
    assert_eq!(releases.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn worker_close_and_in_flight_open_converge_on_one_cleanup() {
    let bidi = FakeBidi::new(vec![Ok(json!({"context": "context-opening-at-close"}))]);
    let marker = bidi
        .block_once("script.evaluate", Some("automation-runtime-binding:"))
        .await;
    let releases = Arc::new(AtomicUsize::new(0));
    let observer = FakeObserver::with_release_counter(observation(), Arc::clone(&releases));
    let worker = Arc::new(worker(bidi.clone(), observer).await);
    let opening_worker = Arc::clone(&worker);
    let opening = tokio::spawn(async move { opening_worker.open_page(PageId::new()).await });
    marker.started.notified().await;

    worker.close().await.unwrap();
    assert_eq!(releases.load(Ordering::SeqCst), 1);
    assert_eq!(
        bidi.calls()
            .await
            .iter()
            .filter(|call| call.method == "browsingContext.close")
            .count(),
        1
    );
    marker.release.notify_one();
    assert!(opening.await.unwrap().is_err());
    tokio::task::yield_now().await;
    assert_eq!(releases.load(Ordering::SeqCst), 1);
    assert_eq!(
        bidi.calls()
            .await
            .iter()
            .filter(|call| call.method == "browsingContext.close")
            .count(),
        1
    );
}

#[tokio::test]
async fn cancelled_close_waiter_and_worker_drop_do_not_cancel_owned_shutdown() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-owned-close-one"})),
        Ok(json!({"context": "context-owned-close-two"})),
    ]);
    let first_close = bidi.block_once("browsingContext.close", None).await;
    let releases = Arc::new(AtomicUsize::new(0));
    let observer = FakeObserver::with_release_counter(observation(), Arc::clone(&releases));
    let worker = Arc::new(worker(bidi.clone(), observer).await);
    worker.open_page(PageId::new()).await.unwrap();
    worker.open_page(PageId::new()).await.unwrap();

    let close_worker = Arc::clone(&worker);
    let close_waiter = tokio::spawn(async move { close_worker.close().await });
    first_close.started.notified().await;
    close_waiter.abort();
    let _ = close_waiter.await;
    drop(worker);
    first_close.release.notify_one();

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let closed_contexts = bidi
                .calls()
                .await
                .iter()
                .filter(|call| call.method == "browsingContext.close")
                .count();
            if closed_contexts == 2
                && releases.load(Ordering::SeqCst) == 2
                && bidi.transport_close_count() == 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owned shutdown must outlive its cancelled waiter and dropped worker");
}

#[tokio::test]
async fn lagged_context_events_resynchronize_and_prune_missing_contexts() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-stale"})),
        Ok(json!({"context": "context-live"})),
        Ok(json!({"url": "https://example.test/live"})),
    ]);
    let observer = FakeObserver::new(observation());
    let releases = Arc::clone(&observer.releases);
    let worker = worker(bidi.clone(), observer).await;
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
    wait_for_release_count(&releases, 1).await;

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
async fn failed_async_binding_release_retries_boundedly_and_fails_the_worker_closed() {
    let bidi = FakeBidi::new(vec![Ok(json!({"context": "context-doomed"}))]);
    let releases = Arc::new(AtomicUsize::new(0));
    let observer = FakeObserver::with_release_error(
        observation(),
        Arc::clone(&releases),
        CommandError {
            code: ErrorCode::BrowserCommandFailed,
            message: "coordinator release failed".into(),
            layer: ErrorLayer::Driver,
            retryable: true,
        },
    );
    let worker = worker(bidi.clone(), observer).await;
    let page = PageId::new();
    worker.open_page(page.clone()).await.unwrap();

    bidi.emit(
        "browsingContext.contextDestroyed",
        json!({"context": "context-doomed"}),
    );
    wait_for_release_count(&releases, 3).await;

    let error = worker
        .open_page(PageId::new())
        .await
        .expect_err("cleanup failure must fail the worker closed");
    assert_eq!(error.code, ErrorCode::BrowserCommandFailed);
    assert!(error.message.contains("coordinator release failed"));
}

#[tokio::test]
async fn hung_binding_release_times_out_each_attempt_and_does_not_stall_later_cleanup() {
    let bidi = FakeBidi::new(vec![
        Ok(json!({"context": "context-hung-release"})),
        Ok(json!({"context": "context-later-release"})),
    ]);
    let attempts = Arc::new(AtomicUsize::new(0));
    let successful_releases = Arc::new(AtomicUsize::new(0));
    let observer = Arc::new(HangingFirstReleaseObserver {
        first_page: Mutex::new(None),
        attempts: Arc::clone(&attempts),
        successful_releases: Arc::clone(&successful_releases),
        timeout: Duration::from_millis(10),
    });
    let worker = FirefoxCompanionWorker::new(
        WorkerId::new(),
        PathBuf::from("/profiles/firefox"),
        lease(),
        bidi.clone(),
        observer,
    )
    .await
    .unwrap();
    let hung_page = PageId::new();
    let later_page = PageId::new();
    worker.open_page(hung_page.clone()).await.unwrap();
    worker.open_page(later_page.clone()).await.unwrap();

    bidi.emit(
        "browsingContext.contextDestroyed",
        json!({"context": "context-hung-release"}),
    );
    bidi.emit(
        "browsingContext.contextDestroyed",
        json!({"context": "context-later-release"}),
    );

    tokio::time::timeout(Duration::from_millis(200), async {
        while successful_releases.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a hung release must not stall the next lifecycle event");
    assert_eq!(attempts.load(Ordering::SeqCst), 4);
    let error = worker
        .open_page(PageId::new())
        .await
        .expect_err("exhausted release deadlines must fail the worker closed");
    assert_eq!(error.code, ErrorCode::BrowserCommandFailed);
    assert!(error.message.contains("3 attempts"));
    assert!(error.message.contains("timed out"));
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

#[derive(Default)]
struct RenewingObserver {
    renewals: AtomicUsize,
}

#[async_trait]
impl ExtensionObserver for RenewingObserver {
    async fn begin_page_binding(
        &self,
        _lease: &AttachmentLease,
        page_id: &PageId,
    ) -> Result<Box<dyn ExtensionPageBinding>, CommandError> {
        let _ = page_id;
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
        Ok(observation())
    }

    async fn release_page_binding(
        &self,
        _lease: &AttachmentLease,
        _page_id: &PageId,
    ) -> Result<(), CommandError> {
        Ok(())
    }

    async fn renew_lease(&self, lease: &AttachmentLease) -> Result<AttachmentLease, CommandError> {
        self.renewals.fetch_add(1, Ordering::SeqCst);
        let mut renewed = lease.clone();
        renewed.expires_at = Instant::now() + Duration::from_secs(300);
        Ok(renewed)
    }
}

#[tokio::test]
async fn worker_renews_attachment_lease_before_expiry() {
    let observer = Arc::new(RenewingObserver::default());
    let mut short_lease = lease();
    short_lease.expires_at = Instant::now() + Duration::from_millis(400);
    let worker = FirefoxCompanionWorker::new(
        WorkerId::new(),
        PathBuf::from("/profiles/firefox"),
        short_lease,
        FakeBidi::new(vec![]),
        observer.clone(),
    )
    .await
    .unwrap();

    tokio::time::timeout(Duration::from_secs(3), async {
        while observer.renewals.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("attachment lease was not renewed before expiry");
    drop(worker);
}

#[tokio::test]
async fn activate_page_sends_bidi_activation_for_the_pages_context() {
    let bidi = FakeBidi::new(vec![]);
    let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();

    worker
        .activate_page(&types::ActivatePageCommand {
            page_id: page_id.clone(),
        })
        .await
        .unwrap();

    let calls = bidi.calls().await;
    assert!(
        calls.iter().any(|call| {
            call.method == "browsingContext.activate" && call.params.get("context").is_some()
        }),
        "{calls:?}"
    );

    let missing = worker
        .activate_page(&types::ActivatePageCommand {
            page_id: PageId::new(),
        })
        .await;
    assert!(missing.is_err());
}

#[tokio::test]
async fn type_text_with_mismatched_expected_url_fails_before_any_native_input() {
    let bidi = FakeBidi::new(vec![]);
    let worker = worker(bidi.clone(), FakeObserver::new(observation())).await;
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();

    let error = worker
        .type_text(
            &page_id,
            &types::TypeTextCommand {
                selector: "#name".into(),
                target: None,
                value: "Ada".into(),
                clear_first: true,
                expected_url: Some("https://wrong.example/".into()),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::VerificationFailed, "{error:?}");
    assert!(error.message.contains("not the expected"));
    assert!(
        !bidi
            .calls()
            .await
            .iter()
            .any(|call| { call.method.contains("type") || call.method.contains("performActions") }),
        "no native input must be dispatched on URL mismatch"
    );
}

#[tokio::test]
async fn handle_dialog_waits_for_and_accepts_a_user_prompt() {
    let bidi = FakeBidi::new(vec![Ok(json!({"context": "context-1"}))]);
    let worker = Arc::new(worker(bidi.clone(), FakeObserver::new(observation())).await);
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();

    let handle = tokio::spawn({
        let worker = Arc::clone(&worker);
        async move {
            worker
                .handle_dialog(
                    &page_id,
                    &types::HandleDialogCommand {
                        action: types::DialogAction::Accept,
                        timeout_ms: Some(2_000),
                    },
                )
                .await
        }
    });
    tokio::task::yield_now().await;
    bidi.emit(
        "browsingContext.userPromptOpened",
        json!({"context": "context-1", "type": "alert", "message": "Leave site?"}),
    );

    let evidence = handle.await.unwrap().unwrap();
    assert!(evidence.iter().any(|item| matches!(
        item,
        Evidence::Dialog { dialog_type, message, action }
            if dialog_type == "alert" && message == "Leave site?" && action == "accept"
    )));
    assert!(bidi.calls().await.iter().any(|call| {
        call.method == "browsingContext.handleUserPrompt"
            && call.params.get("accept") == Some(&json!(true))
    }));
}
