use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
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
const RETIRED_CAPACITY: usize = 256;

struct PendingResponse {
    response: oneshot::Sender<Result<Value, CommandError>>,
    _permit: OwnedSemaphorePermit,
}

#[derive(Default)]
struct CorrelationState {
    pending: HashMap<u64, PendingResponse>,
    retired: VecDeque<u64>,
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
    next_id: Arc<AtomicU64>,
    enqueue: Arc<Mutex<()>>,
    permits: Arc<Semaphore>,
    events: broadcast::Sender<BidiEvent>,
    timeout: Duration,
}

struct SharedState {
    correlations: Correlations,
    terminal: Mutex<Option<TerminalFailure>>,
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
            correlations: Arc::new(Mutex::new(CorrelationState {
                pending: HashMap::with_capacity(COMMAND_CAPACITY),
                retired: VecDeque::with_capacity(RETIRED_CAPACITY),
            })),
            terminal: Mutex::new(None),
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
            next_id: Arc::new(AtomicU64::new(1)),
            enqueue: Arc::new(Mutex::new(())),
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
        if let Some(error) = terminal_error(&self.shared).await {
            return Err(error);
        }
        let permit = tokio::time::timeout_at(deadline, self.permits.clone().acquire_owned())
            .await
            .map_err(|_| deadline_error("Firefox BiDi command deadline exceeded"))?
            .map_err(|_| transport_failure("Firefox BiDi command capacity closed").error())?;
        let enqueue = tokio::time::timeout_at(deadline, self.enqueue.lock())
            .await
            .map_err(|_| deadline_error("Firefox BiDi command deadline exceeded"))?;
        if let Some(error) = terminal_error(&self.shared).await {
            return Err(error);
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (response_tx, response_rx) = oneshot::channel();
        {
            let mut correlations = self.shared.correlations.lock().await;
            debug_assert!(correlations.pending.len() < COMMAND_CAPACITY);
            correlations.pending.insert(
                id,
                PendingResponse {
                    response: response_tx,
                    _permit: permit,
                },
            );
        }
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
                return Err(terminal_error(&self.shared).await.unwrap_or_else(|| {
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
                Err(terminal_error(&self.shared).await.unwrap_or_else(|| {
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
            } else if let Some(index) = correlations
                .retired
                .iter()
                .position(|retired| *retired == id)
            {
                correlations.retired.remove(index);
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
    let Some(params) = value.get("params").cloned() else {
        terminate(
            shared,
            protocol_failure("Firefox BiDi event params were missing"),
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
            .cloned()
            .map(Ok)
            .ok_or_else(|| protocol_failure("Firefox BiDi success response omitted result")),
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
    if correlations.pending.remove(&id).is_none() {
        return;
    }
    if correlations.retired.len() == RETIRED_CAPACITY {
        correlations.retired.pop_front();
    }
    correlations.retired.push_back(id);
}

async fn terminal_error(shared: &SharedState) -> Option<CommandError> {
    shared
        .terminal
        .lock()
        .await
        .as_ref()
        .map(TerminalFailure::error)
}

async fn terminate(shared: &SharedState, failure: TerminalFailure) {
    let effective = {
        let mut terminal = shared.terminal.lock().await;
        terminal.get_or_insert(failure).clone()
    };
    shared.closing.store(true, Ordering::Release);
    let _ = shared.close_signal.send(true);
    let pending = {
        let mut correlations = shared.correlations.lock().await;
        correlations.retired.clear();
        std::mem::take(&mut correlations.pending)
    };
    for (_, response) in pending {
        let _ = response.response.send(Err(effective.error()));
    }
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
        assert!(correlations.retired.len() <= RETIRED_CAPACITY);
        assert_eq!(permits.available_permits(), COMMAND_CAPACITY);
    }
}
