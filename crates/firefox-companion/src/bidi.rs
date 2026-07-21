use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, Semaphore};
use tokio::time::Instant;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use types::{CommandError, ErrorCode, ErrorLayer};
use url::Url;

const COMMAND_CAPACITY: usize = 64;
const EVENT_CAPACITY: usize = 64;

type PendingResponse = oneshot::Sender<Result<Value, CommandError>>;
type PendingResponses = Arc<Mutex<HashMap<u64, PendingResponse>>>;

#[derive(Debug, Clone, PartialEq)]
pub struct BidiEvent {
    pub method: String,
    pub params: Value,
}

#[derive(Clone)]
pub struct BidiClient {
    commands: mpsc::Sender<WriterCommand>,
    pending: PendingResponses,
    terminal: Arc<Mutex<Option<TerminalFailure>>>,
    next_id: Arc<AtomicU64>,
    enqueue: Arc<Mutex<()>>,
    permits: Arc<Semaphore>,
    events: broadcast::Sender<BidiEvent>,
    timeout: Duration,
}

enum WriterCommand {
    Send {
        id: u64,
        method: String,
        params: Value,
    },
    Close {
        completion: oneshot::Sender<Result<(), String>>,
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
        let (mut writer, mut reader) = socket.split();
        let (commands, mut command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let pending = Arc::new(Mutex::new(HashMap::with_capacity(COMMAND_CAPACITY)));
        let terminal = Arc::new(Mutex::new(None));

        let writer_pending = Arc::clone(&pending);
        let writer_terminal = Arc::clone(&terminal);
        tokio::spawn(async move {
            while let Some(command) = command_rx.recv().await {
                match command {
                    WriterCommand::Send { id, method, params } => {
                        let payload = json!({"id": id, "method": method, "params": params});
                        if let Err(error) =
                            writer.send(Message::Text(payload.to_string().into())).await
                        {
                            fail_all(
                                &writer_pending,
                                &writer_terminal,
                                transport_failure(format!(
                                    "Firefox BiDi writer disconnected: {error}"
                                )),
                            )
                            .await;
                            return;
                        }
                    }
                    WriterCommand::Close { completion } => {
                        let result = writer.close().await.map_err(|error| error.to_string());
                        let _ = completion.send(result);
                        return;
                    }
                }
            }
            let _ = writer.close().await;
        });

        let reader_pending = Arc::clone(&pending);
        let reader_terminal = Arc::clone(&terminal);
        let reader_events = events.clone();
        tokio::spawn(async move {
            while let Some(message) = reader.next().await {
                let message = match message {
                    Ok(message) => message,
                    Err(error) => {
                        fail_all(
                            &reader_pending,
                            &reader_terminal,
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
                                fail_all(
                                    &reader_pending,
                                    &reader_terminal,
                                    protocol_failure(format!(
                                        "Firefox BiDi sent invalid JSON: {error}"
                                    )),
                                )
                                .await;
                                return;
                            }
                        };
                        if let Some(id) = value.get("id") {
                            let Some(id) = id.as_u64() else {
                                fail_all(
                                    &reader_pending,
                                    &reader_terminal,
                                    protocol_failure("Firefox BiDi response ID was not an integer"),
                                )
                                .await;
                                return;
                            };
                            let response = reader_pending.lock().await.remove(&id);
                            let Some(response) = response else {
                                fail_all(
                                    &reader_pending,
                                    &reader_terminal,
                                    protocol_failure(format!(
                                        "Firefox BiDi returned unknown response ID {id}"
                                    )),
                                )
                                .await;
                                return;
                            };
                            let result = response_result(value);
                            let _ = response.send(result);
                        } else if let Some(method) = value.get("method").and_then(Value::as_str) {
                            let _ = reader_events.send(BidiEvent {
                                method: method.to_owned(),
                                params: value.get("params").cloned().unwrap_or(Value::Null),
                            });
                        } else {
                            fail_all(
                                &reader_pending,
                                &reader_terminal,
                                protocol_failure(
                                    "Firefox BiDi message had neither a response ID nor an event method",
                                ),
                            )
                            .await;
                            return;
                        }
                    }
                    Message::Close(_) => {
                        fail_all(
                            &reader_pending,
                            &reader_terminal,
                            transport_failure("Firefox BiDi connection closed"),
                        )
                        .await;
                        return;
                    }
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                    Message::Binary(_) => {
                        fail_all(
                            &reader_pending,
                            &reader_terminal,
                            protocol_failure("Firefox BiDi sent an unsupported binary message"),
                        )
                        .await;
                        return;
                    }
                }
            }
            fail_all(
                &reader_pending,
                &reader_terminal,
                transport_failure("Firefox BiDi connection ended"),
            )
            .await;
        });

        Ok(Self {
            commands,
            pending,
            terminal,
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
        if let Some(error) = terminal_error(&self.terminal).await {
            return Err(error);
        }
        let _permit = tokio::time::timeout_at(deadline, self.permits.clone().acquire_owned())
            .await
            .map_err(|_| deadline_error("Firefox BiDi command deadline exceeded"))?
            .map_err(|_| transport_failure("Firefox BiDi command channel closed").error())?;
        let enqueue = tokio::time::timeout_at(deadline, self.enqueue.lock())
            .await
            .map_err(|_| deadline_error("Firefox BiDi command deadline exceeded"))?;
        if let Some(error) = terminal_error(&self.terminal).await {
            return Err(error);
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (response_tx, response_rx) = oneshot::channel();
        self.pending.lock().await.insert(id, response_tx);
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
                self.pending.lock().await.remove(&id);
                return Err(terminal_error(&self.terminal).await.unwrap_or_else(|| {
                    transport_failure("Firefox BiDi command channel closed").error()
                }));
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                return Err(deadline_error("Firefox BiDi command deadline exceeded"));
            }
        }

        match tokio::time::timeout_at(deadline, response_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(terminal_error(&self.terminal).await.unwrap_or_else(|| {
                transport_failure("Firefox BiDi response channel closed").error()
            })),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(deadline_error("Firefox BiDi command deadline exceeded"))
            }
        }
    }

    pub async fn close(&self) -> Result<(), CommandError> {
        let enqueue = tokio::time::timeout(self.timeout, self.enqueue.lock())
            .await
            .map_err(|_| deadline_error("Firefox BiDi close deadline exceeded"))?;
        {
            let terminal = self.terminal.lock().await;
            if terminal.is_some() {
                return Ok(());
            }
        }
        fail_all(
            &self.pending,
            &self.terminal,
            TerminalFailure {
                code: ErrorCode::BrowserCommandFailed,
                message: "Firefox BiDi client closed".into(),
                retryable: false,
            },
        )
        .await;
        let (completion, completed) = oneshot::channel();
        tokio::time::timeout(
            self.timeout,
            self.commands.send(WriterCommand::Close { completion }),
        )
        .await
        .map_err(|_| deadline_error("Firefox BiDi close deadline exceeded"))?
        .map_err(|_| transport_failure("Firefox BiDi command channel closed").error())?;
        drop(enqueue);
        tokio::time::timeout(self.timeout, completed)
            .await
            .map_err(|_| deadline_error("Firefox BiDi close deadline exceeded"))?
            .map_err(|_| transport_failure("Firefox BiDi writer stopped before close").error())?
            .map_err(|message| CommandError {
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

fn response_result(value: Value) -> Result<Value, CommandError> {
    if value.get("type").and_then(Value::as_str) == Some("error") || value.get("error").is_some() {
        let code = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Firefox BiDi command failed");
        return Err(CommandError {
            code: ErrorCode::BrowserCommandFailed,
            message: format!("Firefox BiDi {code}: {message}"),
            layer: ErrorLayer::Driver,
            retryable: false,
        });
    }
    Ok(value.get("result").cloned().unwrap_or(Value::Null))
}

async fn terminal_error(terminal: &Mutex<Option<TerminalFailure>>) -> Option<CommandError> {
    terminal.lock().await.as_ref().map(TerminalFailure::error)
}

async fn fail_all(
    pending: &PendingResponses,
    terminal: &Mutex<Option<TerminalFailure>>,
    failure: TerminalFailure,
) {
    let effective = {
        let mut terminal = terminal.lock().await;
        terminal.get_or_insert(failure).clone()
    };
    let pending = std::mem::take(&mut *pending.lock().await);
    for (_, response) in pending {
        let _ = response.send(Err(effective.error()));
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
