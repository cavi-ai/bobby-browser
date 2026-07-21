use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex as SyncMutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{
    broadcast, mpsc, oneshot, watch, Mutex, Notify, OwnedSemaphorePermit, Semaphore,
};
use tokio::time::Instant;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use types::{CommandError, ErrorCode, ErrorLayer};
use url::Url;

const COMMAND_CAPACITY: usize = 64;
const EVENT_CAPACITY: usize = 64;

struct PendingResponse {
    response: oneshot::Sender<Result<Value, CommandError>>,
    _permit: OwnedSemaphorePermit,
}

struct CorrelationState {
    pending: HashMap<u64, PendingResponse>,
    next_id: u64,
}

impl Default for CorrelationState {
    fn default() -> Self {
        Self {
            pending: HashMap::with_capacity(COMMAND_CAPACITY),
            next_id: 1,
        }
    }
}

type Correlations = Arc<Mutex<CorrelationState>>;

#[derive(Debug, Clone, PartialEq)]
pub struct BidiEvent {
    pub method: String,
    pub params: Value,
}

#[derive(Clone)]
pub struct BidiClient {
    commands: mpsc::Sender<WriterCommand>,
    shared: Arc<SharedState>,
    permits: Arc<Semaphore>,
    events: broadcast::Sender<BidiEvent>,
    timeout: Duration,
}

struct SharedState {
    correlations: Correlations,
    enqueue: Mutex<()>,
    terminal: SyncMutex<Option<TerminalFailure>>,
    closing: AtomicBool,
    close_signal: watch::Sender<bool>,
    writer_result: Mutex<Option<Result<(), String>>>,
    writer_done: Notify,
}

enum WriterCommand {
    Send {
        id: u64,
        method: String,
        params: Value,
    },
}

#[derive(Clone)]
struct TerminalFailure {
    code: ErrorCode,
    message: String,
    retryable: bool,
}

impl TerminalFailure {
    fn error(&self) -> CommandError {
        CommandError {
            code: self.code,
            message: self.message.clone(),
            layer: ErrorLayer::Driver,
            retryable: self.retryable,
        }
    }
}

struct PendingGuard {
    correlations: Correlations,
    id: Option<u64>,
}

impl PendingGuard {
    fn new(correlations: Correlations, id: u64) -> Self {
        Self {
            correlations,
            id: Some(id),
        }
    }

    fn disarm(&mut self) {
        self.id = None;
    }

    async fn retire(&mut self) {
        let Some(id) = self.id.take() else {
            return;
        };
        let mut correlations = self.correlations.lock().await;
        retire_correlation(&mut correlations, id);
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else {
            return;
        };
        if let Ok(mut correlations) = self.correlations.try_lock() {
            retire_correlation(&mut correlations, id);
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let correlations = Arc::clone(&self.correlations);
        runtime.spawn(async move {
            let mut correlations = correlations.lock().await;
            retire_correlation(&mut correlations, id);
        });
    }
}

impl BidiClient {
    pub async fn connect(url: Url, timeout: Duration) -> Result<Self, CommandError> {
        let (socket, _) = tokio::time::timeout(timeout, connect_async(url.as_str()))
            .await
            .map_err(|_| deadline_error("BiDi connection deadline exceeded"))?
            .map_err(|error| CommandError {
                code: ErrorCode::BrowserLaunchFailed,
                message: format!("failed to connect to Firefox BiDi: {error}"),
                layer: ErrorLayer::Driver,
                retryable: true,
            })?;
        let (writer, reader) = socket.split();
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let (close_signal, close_rx) = watch::channel(false);
        let shared = Arc::new(SharedState {
            correlations: Arc::new(Mutex::new(CorrelationState::default())),
            enqueue: Mutex::new(()),
            terminal: SyncMutex::new(None),
            closing: AtomicBool::new(false),
            close_signal,
            writer_result: Mutex::new(None),
            writer_done: Notify::new(),
        });

        tokio::spawn(writer_task(
            writer,
            command_rx,
            close_rx,
            Arc::clone(&shared),
        ));
        tokio::spawn(reader_task(reader, Arc::clone(&shared), events.clone()));

        Ok(Self {
            commands,
            shared,
            permits: Arc::new(Semaphore::new(COMMAND_CAPACITY)),
            events,
            timeout,
        })
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<BidiEvent> {
        self.events.subscribe()
    }

    pub async fn send(&self, method: &str, params: Value) -> Result<Value, CommandError> {
        let deadline = Instant::now() + self.timeout;
        if let Some(error) = terminal_error(&self.shared) {
            return Err(error);
        }
        let permit = tokio::time::timeout_at(deadline, self.permits.clone().acquire_owned())
            .await
            .map_err(|_| deadline_error("Firefox BiDi command deadline exceeded"))?
            .map_err(|_| transport_failure("Firefox BiDi command capacity closed").error())?;
        let enqueue = tokio::time::timeout_at(deadline, self.shared.enqueue.lock())
            .await
            .map_err(|_| deadline_error("Firefox BiDi command deadline exceeded"))?;
        let (response_tx, response_rx) = oneshot::channel();
        let id = {
            let mut correlations = self.shared.correlations.lock().await;
            let terminal = self
                .shared
                .terminal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(failure) = terminal.as_ref() {
                return Err(failure.error());
            }
            let id = correlations.next_id;
            correlations.next_id = correlations.next_id.checked_add(1).ok_or_else(|| {
                protocol_failure("Firefox BiDi command ID space was exhausted").error()
            })?;
            debug_assert!(correlations.pending.len() < COMMAND_CAPACITY);
            correlations.pending.insert(
                id,
                PendingResponse {
                    response: response_tx,
                    _permit: permit,
                },
            );
            id
        };
        let mut guard = PendingGuard::new(Arc::clone(&self.shared.correlations), id);
        let outbound = WriterCommand::Send {
            id,
            method: method.to_owned(),
            params,
        };
        let sent = tokio::time::timeout_at(deadline, self.commands.send(outbound)).await;
        drop(enqueue);
        match sent {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                guard.retire().await;
                return Err(terminal_error(&self.shared).unwrap_or_else(|| {
                    transport_failure("Firefox BiDi command channel closed").error()
                }));
            }
            Err(_) => {
                guard.retire().await;
                return Err(deadline_error("Firefox BiDi command deadline exceeded"));
            }
        }

        match tokio::time::timeout_at(deadline, response_rx).await {
            Ok(Ok(result)) => {
                guard.disarm();
                result
            }
            Ok(Err(_)) => {
                guard.retire().await;
                Err(terminal_error(&self.shared).unwrap_or_else(|| {
                    transport_failure("Firefox BiDi response channel closed").error()
                }))
            }
            Err(_) => {
                guard.retire().await;
                Err(deadline_error("Firefox BiDi command deadline exceeded"))
            }
        }
    }

    pub async fn close(&self) -> Result<(), CommandError> {
        terminate(
            &self.shared,
            TerminalFailure {
                code: ErrorCode::BrowserCommandFailed,
                message: "Firefox BiDi client closed".into(),
                retryable: false,
            },
        )
        .await;
        let result = tokio::time::timeout(self.timeout, wait_for_writer(&self.shared))
            .await
            .map_err(|_| deadline_error("Firefox BiDi close deadline exceeded"))?;
        result.map_err(|message| CommandError {
            code: ErrorCode::BrowserCommandFailed,
            message: format!("failed to close Firefox BiDi: {message}"),
            layer: ErrorLayer::Driver,
            retryable: true,
        })
    }
}

#[async_trait]
pub trait BidiTransport: Send + Sync {
    async fn send(&self, method: &str, params: Value) -> Result<Value, CommandError>;

    fn subscribe_events(&self) -> Option<broadcast::Receiver<BidiEvent>> {
        None
    }

    async fn close(&self) -> Result<(), CommandError> {
        Ok(())
    }
}

#[async_trait]
impl BidiTransport for BidiClient {
    async fn send(&self, method: &str, params: Value) -> Result<Value, CommandError> {
        BidiClient::send(self, method, params).await
    }

    fn subscribe_events(&self) -> Option<broadcast::Receiver<BidiEvent>> {
        Some(BidiClient::subscribe_events(self))
    }

    async fn close(&self) -> Result<(), CommandError> {
        BidiClient::close(self).await
    }
}

async fn writer_task<S>(
    mut writer: S,
    mut commands: mpsc::Receiver<WriterCommand>,
    mut close_rx: watch::Receiver<bool>,
    shared: Arc<SharedState>,
) where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let mut transport_error = None;
    'writer: loop {
        if shared.closing.load(Ordering::Acquire) {
            break;
        }
        let command = tokio::select! {
            biased;
            changed = close_rx.changed() => {
                if changed.is_err() || *close_rx.borrow() {
                    break 'writer;
                }
                continue;
            }
            command = commands.recv() => {
                let Some(command) = command else { break 'writer; };
                command
            }
        };
        if shared.closing.load(Ordering::Acquire) {
            break;
        }
        let WriterCommand::Send { id, method, params } = command;
        let payload = json!({"id": id, "method": method, "params": params});
        let sent = tokio::select! {
            biased;
            changed = close_rx.changed() => {
                if changed.is_err() || *close_rx.borrow() {
                    break 'writer;
                }
                continue;
            }
            result = writer.send(Message::Text(payload.to_string().into())) => result,
        };
        if let Err(error) = sent {
            let message = format!("Firefox BiDi writer disconnected: {error}");
            terminate(&shared, transport_failure(message.clone())).await;
            transport_error = Some(message);
            break;
        }
    }
    commands.close();
    drop(commands);
    let close_result = writer.close().await.map_err(|error| error.to_string());
    let result = transport_error.map_or(close_result, Err);
    *shared.writer_result.lock().await = Some(result);
    shared.writer_done.notify_waiters();
}

async fn reader_task<S>(
    mut reader: S,
    shared: Arc<SharedState>,
    events: broadcast::Sender<BidiEvent>,
) where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    while let Some(message) = reader.next().await {
        let message = match message {
            Ok(message) => message,
            Err(error) => {
                terminate(
                    &shared,
                    transport_failure(format!("Firefox BiDi reader disconnected: {error}")),
                )
                .await;
                return;
            }
        };
        match message {
            Message::Text(text) => {
                let value: Value = match serde_json::from_str(text.as_str()) {
                    Ok(value) => value,
                    Err(error) => {
                        terminate(
                            &shared,
                            protocol_failure(format!("Firefox BiDi sent invalid JSON: {error}")),
                        )
                        .await;
                        return;
                    }
                };
                if handle_message(value, &shared, &events).await.is_err() {
                    return;
                }
            }
            Message::Close(_) => {
                terminate(&shared, transport_failure("Firefox BiDi connection closed")).await;
                return;
            }
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            Message::Binary(_) => {
                terminate(
                    &shared,
                    protocol_failure("Firefox BiDi sent an unsupported binary message"),
                )
                .await;
                return;
            }
        }
    }
    terminate(&shared, transport_failure("Firefox BiDi connection ended")).await;
}

async fn handle_message(
    value: Value,
    shared: &SharedState,
    events: &broadcast::Sender<BidiEvent>,
) -> Result<(), ()> {
    if value.get("id").is_some() {
        let Some(id) = value.get("id").and_then(Value::as_u64) else {
            terminate(
                shared,
                protocol_failure("Firefox BiDi response ID was not an unsigned integer"),
            )
            .await;
            return Err(());
        };
        let result = match response_result(&value) {
            Ok(result) => result,
            Err(failure) => {
                terminate(shared, failure).await;
                return Err(());
            }
        };
        let response = {
            let mut correlations = shared.correlations.lock().await;
            if let Some(response) = correlations.pending.remove(&id) {
                Some(response)
            } else if id > 0 && id < correlations.next_id {
                return Ok(());
            } else {
                None
            }
        };
        let Some(response) = response else {
            terminate(
                shared,
                protocol_failure(format!("Firefox BiDi returned unknown response ID {id}")),
            )
            .await;
            return Err(());
        };
        let _ = response.response.send(result);
        return Ok(());
    }

    if value.get("type").and_then(Value::as_str) != Some("event") {
        terminate(
            shared,
            protocol_failure("Firefox BiDi event type was missing or invalid"),
        )
        .await;
        return Err(());
    }
    let Some(method) = value
        .get("method")
        .and_then(Value::as_str)
        .filter(|method| !method.is_empty())
    else {
        terminate(
            shared,
            protocol_failure("Firefox BiDi event method was missing or invalid"),
        )
        .await;
        return Err(());
    };
    let Some(params) = value
        .get("params")
        .filter(|params| params.is_object())
        .cloned()
    else {
        terminate(
            shared,
            protocol_failure("Firefox BiDi event params were missing or not an object"),
        )
        .await;
        return Err(());
    };
    let _ = events.send(BidiEvent {
        method: method.to_owned(),
        params,
    });
    Ok(())
}

fn response_result(value: &Value) -> Result<Result<Value, CommandError>, TerminalFailure> {
    match value.get("type").and_then(Value::as_str) {
        Some("success") => value
            .get("result")
            .filter(|result| result.is_object())
            .cloned()
            .map(Ok)
            .ok_or_else(|| {
                protocol_failure(
                    "Firefox BiDi success response result was missing or not an object",
                )
            }),
        Some("error") => {
            let code = value
                .get("error")
                .and_then(Value::as_str)
                .filter(|code| !code.is_empty())
                .ok_or_else(|| protocol_failure("Firefox BiDi error response omitted error"))?;
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .ok_or_else(|| protocol_failure("Firefox BiDi error response omitted message"))?;
            Ok(Err(CommandError {
                code: ErrorCode::BrowserCommandFailed,
                message: format!("Firefox BiDi {code}: {message}"),
                layer: ErrorLayer::Driver,
                retryable: false,
            }))
        }
        _ => Err(protocol_failure(
            "Firefox BiDi response type was missing or invalid",
        )),
    }
}

fn retire_correlation(correlations: &mut CorrelationState, id: u64) {
    correlations.pending.remove(&id);
}

fn terminal_error(shared: &SharedState) -> Option<CommandError> {
    shared
        .terminal
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(TerminalFailure::error)
}

async fn terminate(shared: &SharedState, failure: TerminalFailure) {
    let effective = {
        let mut terminal = shared
            .terminal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        terminal.get_or_insert(failure).clone()
    };
    shared.closing.store(true, Ordering::Release);
    let _ = shared.close_signal.send(true);

    let correlations = Arc::clone(&shared.correlations);
    let draining = tokio::spawn(async move {
        let mut correlations = correlations.lock().await;
        let pending = std::mem::take(&mut correlations.pending);
        drop(correlations);
        for (_, response) in pending {
            let _ = response.response.send(Err(effective.error()));
        }
    });
    let _ = draining.await;
}

async fn wait_for_writer(shared: &SharedState) -> Result<(), String> {
    loop {
        let notified = shared.writer_done.notified();
        if let Some(result) = shared.writer_result.lock().await.clone() {
            return result;
        }
        notified.await;
    }
}

fn deadline_error(message: impl Into<String>) -> CommandError {
    CommandError {
        code: ErrorCode::DeadlineExceeded,
        message: message.into(),
        layer: ErrorLayer::Driver,
        retryable: true,
    }
}

fn transport_failure(message: impl Into<String>) -> TerminalFailure {
    TerminalFailure {
        code: ErrorCode::BrowserCommandFailed,
        message: message.into(),
        retryable: true,
    }
}

fn protocol_failure(message: impl Into<String>) -> TerminalFailure {
    TerminalFailure {
        code: ErrorCode::BrowserCommandFailed,
        message: message.into(),
        retryable: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        pin::Pin,
        sync::{atomic::AtomicUsize, Mutex as StdMutex},
        task::{Context, Poll, Waker},
    };

    struct NeverReadySink {
        ready_polled: Arc<Notify>,
        allow_close: Arc<AtomicBool>,
        close_waker: Arc<StdMutex<Option<Waker>>>,
        sent: Arc<AtomicUsize>,
    }

    impl futures_util::Sink<Message> for NeverReadySink {
        type Error = tokio_tungstenite::tungstenite::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            self.ready_polled.notify_one();
            Poll::Pending
        }

        fn start_send(self: Pin<&mut Self>, _item: Message) -> Result<(), Self::Error> {
            self.sent.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            if self.allow_close.load(Ordering::Acquire) {
                Poll::Ready(Ok(()))
            } else {
                *self.close_waker.lock().expect("close waker mutex poisoned") =
                    Some(context.waker().clone());
                Poll::Pending
            }
        }
    }

    fn test_shared() -> SharedState {
        let (close_signal, _) = watch::channel(false);
        SharedState {
            correlations: Arc::new(Mutex::new(CorrelationState::default())),
            enqueue: Mutex::new(()),
            terminal: SyncMutex::new(None),
            closing: AtomicBool::new(false),
            close_signal,
            writer_result: Mutex::new(None),
            writer_done: Notify::new(),
        }
    }

    fn test_client() -> (BidiClient, mpsc::Receiver<WriterCommand>) {
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        (
            BidiClient {
                commands,
                shared: Arc::new(test_shared()),
                permits: Arc::new(Semaphore::new(COMMAND_CAPACITY)),
                events,
                timeout: Duration::from_secs(1),
            },
            receiver,
        )
    }

    #[tokio::test]
    async fn cancellation_retirement_keeps_live_correlations_within_the_command_bound() {
        let permits = Arc::new(Semaphore::new(COMMAND_CAPACITY));
        let mut correlations = CorrelationState::default();
        for id in 0..(COMMAND_CAPACITY * 4) as u64 {
            let permit = permits.clone().acquire_owned().await.unwrap();
            let (response, _receiver) = oneshot::channel();
            correlations.pending.insert(
                id,
                PendingResponse {
                    response,
                    _permit: permit,
                },
            );
            retire_correlation(&mut correlations, id);
            assert!(correlations.pending.len() <= COMMAND_CAPACITY);
        }
        assert!(correlations.pending.is_empty());
        assert_eq!(permits.available_permits(), COMMAND_CAPACITY);
    }

    #[test]
    fn success_results_must_be_json_objects() {
        for result in [Value::Null, json!("not-a-map"), json!([])] {
            assert!(response_result(&json!({
                "id": 1,
                "type": "success",
                "result": result,
            }))
            .is_err());
        }
    }

    #[tokio::test]
    async fn event_params_must_be_json_objects() {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        for params in [Value::Null, json!("not-a-map"), json!([])] {
            let shared = test_shared();
            assert!(handle_message(
                json!({
                    "type": "event",
                    "method": "log.entryAdded",
                    "params": params,
                }),
                &shared,
                &events,
            )
            .await
            .is_err());
        }
    }

    #[tokio::test]
    async fn first_issued_late_response_survives_more_than_the_old_tombstone_capacity() {
        let shared = test_shared();
        let permits = Arc::new(Semaphore::new(COMMAND_CAPACITY));
        for id in 1..=257 {
            let permit = permits.clone().acquire_owned().await.unwrap();
            let (response, _receiver) = oneshot::channel();
            let mut correlations = shared.correlations.lock().await;
            correlations.pending.insert(
                id,
                PendingResponse {
                    response,
                    _permit: permit,
                },
            );
            correlations.next_id = id + 1;
            retire_correlation(&mut correlations, id);
        }
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        assert!(handle_message(
            json!({"id": 1, "type": "success", "result": {"late": true}}),
            &shared,
            &events,
        )
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn never_issued_zero_response_id_is_rejected() {
        let shared = test_shared();
        shared.correlations.lock().await.next_id = 2;
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        assert!(handle_message(
            json!({"id": 0, "type": "success", "result": {}}),
            &shared,
            &events,
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn terminal_publication_wins_the_registration_race_and_releases_every_permit() {
        let (client, mut commands) = test_client();
        let gate = client.shared.enqueue.lock().await;
        let shared = Arc::clone(&client.shared);
        let terminating = tokio::spawn(async move {
            terminate(&shared, transport_failure("deterministic shutdown")).await;
        });
        tokio::task::yield_now().await;
        let sending_client = client.clone();
        let sending = tokio::spawn(async move {
            sending_client
                .send("effect.after-terminal", json!({}))
                .await
        });
        tokio::task::yield_now().await;
        drop(gate);

        terminating.await.unwrap();
        let error = sending.await.unwrap().unwrap_err();
        assert_eq!(error.message, "deterministic shutdown");
        assert!(commands.try_recv().is_err());
        assert_eq!(client.permits.available_permits(), COMMAND_CAPACITY);

        let (loaded, _commands) = test_client();
        let mut waiting = Vec::with_capacity(COMMAND_CAPACITY);
        for index in 0..COMMAND_CAPACITY {
            let sender = loaded.clone();
            waiting.push(tokio::spawn(async move {
                sender.send("queued.effect", json!({"index": index})).await
            }));
        }
        loop {
            if loaded.shared.correlations.lock().await.pending.len() == COMMAND_CAPACITY {
                break;
            }
            tokio::task::yield_now().await;
        }
        terminate(&loaded.shared, transport_failure("all pending failed")).await;
        tokio::time::timeout(Duration::from_millis(100), async {
            for task in waiting {
                assert_eq!(
                    task.await.unwrap().unwrap_err().message,
                    "all pending failed"
                );
            }
        })
        .await
        .expect("all terminal responses must resolve immediately");
        assert_eq!(loaded.permits.available_permits(), COMMAND_CAPACITY);
    }

    #[tokio::test]
    async fn close_cancellation_preempts_a_full_queue_and_unblocks_its_sender() {
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let shared = Arc::new(test_shared());
        let permits = Arc::new(Semaphore::new(COMMAND_CAPACITY));
        let client = BidiClient {
            commands,
            shared: Arc::clone(&shared),
            permits: Arc::clone(&permits),
            events,
            timeout: Duration::from_secs(2),
        };
        let ready_polled = Arc::new(Notify::new());
        let allow_close = Arc::new(AtomicBool::new(false));
        let close_waker = Arc::new(StdMutex::new(None));
        let sent = Arc::new(AtomicUsize::new(0));
        let writer = tokio::spawn(writer_task(
            NeverReadySink {
                ready_polled: Arc::clone(&ready_polled),
                allow_close: Arc::clone(&allow_close),
                close_waker: Arc::clone(&close_waker),
                sent: Arc::clone(&sent),
            },
            command_rx,
            shared.close_signal.subscribe(),
            Arc::clone(&shared),
        ));

        let blocker_client = client.clone();
        let blocker = tokio::spawn(async move {
            blocker_client
                .send("effect.blocking-writer", json!({}))
                .await
        });
        ready_polled.notified().await;
        blocker.abort();
        let _ = blocker.await;
        while permits.available_permits() != COMMAND_CAPACITY {
            tokio::task::yield_now().await;
        }

        let mut abandoned = Vec::with_capacity(COMMAND_CAPACITY);
        for index in 0..COMMAND_CAPACITY {
            let sender = client.clone();
            abandoned.push(tokio::spawn(async move {
                sender
                    .send("effect.abandoned", json!({"index": index}))
                    .await
            }));
        }
        while client.commands.capacity() != 0 {
            tokio::task::yield_now().await;
        }
        for task in abandoned {
            task.abort();
            let _ = task.await;
        }
        while permits.available_permits() != COMMAND_CAPACITY {
            tokio::task::yield_now().await;
        }

        let blocked_client = client.clone();
        let blocked = tokio::spawn(async move {
            blocked_client
                .send("effect.blocked-on-full-queue", json!({}))
                .await
        });
        loop {
            if shared.correlations.lock().await.pending.len() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }

        let closing_client = client.clone();
        let closing = tokio::spawn(async move { closing_client.close().await });
        tokio::time::timeout(Duration::from_millis(100), async {
            while !shared.closing.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("terminal state must publish without waiting for the full-queue sender");
        closing.abort();
        let _ = closing.await;

        let error = tokio::time::timeout(Duration::from_millis(100), blocked)
            .await
            .expect("dropping the writer receiver must unblock the full-queue sender")
            .unwrap()
            .unwrap_err();
        assert_eq!(error.message, "Firefox BiDi client closed");
        assert_eq!(permits.available_permits(), COMMAND_CAPACITY);
        assert_eq!(sent.load(Ordering::SeqCst), 0);

        allow_close.store(true, Ordering::Release);
        if let Some(waker) = close_waker
            .lock()
            .expect("close waker mutex poisoned")
            .take()
        {
            waker.wake();
        }
        writer.await.unwrap();
    }
}
