//! Bounded, protocol-neutral newline transport for shared local gateways.
use anyhow::{bail, Context, Result};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue, Message};

pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> Result<Option<String>> {
    let mut bytes = Vec::new();
    let length = reader
        .take((MAX_FRAME_BYTES + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .await?;
    if length == 0 {
        return Ok(None);
    }
    if length > MAX_FRAME_BYTES {
        bail!("gateway frame exceeds the 1 MiB limit");
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    Ok(Some(
        String::from_utf8(bytes).context("gateway frame is not UTF-8")?,
    ))
}

pub async fn bridge<R, W, I, O, E>(
    reader: R,
    mut writer: W,
    mut incoming: I,
    mut outgoing: O,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    I: Stream<Item = Result<String, E>> + Unpin,
    O: Sink<String, Error = E> + Unpin,
    E: std::error::Error + Send + Sync + 'static,
{
    let upload = async {
        let mut reader = BufReader::new(reader);
        while let Some(frame) = read_frame(&mut reader).await? {
            if !frame.is_empty() {
                outgoing.send(frame).await?;
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    let download = async {
        while let Some(frame) = incoming.next().await {
            let frame = frame?;
            if frame.len() > MAX_FRAME_BYTES || frame.contains('\n') || frame.contains('\r') {
                bail!("invalid gateway frame");
            }
            writer.write_all(frame.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
        }
        writer.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    };
    let result = tokio::select! { result = upload => result, result = download => result };
    // EOF and owner shutdown must send a close frame rather than dropping
    // the upgraded socket and surfacing a reset to the other adapter.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), outgoing.close()).await;
    result
}

/// Longest a retryable refusal is retried before the refusal is reported.
pub const REFUSAL_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);
/// Shortest wait between attempts, so a `retryAfterMs` of 0 cannot spin.
const MIN_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(10);
/// How long the host gets to have its `initialize` request read after the
/// runtime's final refusal.
const INITIALIZE_READ_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

pub async fn connect<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    origin: &str,
    protocol: &str,
    bearer: &str,
    reader: R,
    writer: W,
) -> Result<()> {
    connect_within(
        origin,
        protocol,
        bearer,
        reader,
        writer,
        REFUSAL_RETRY_BUDGET,
    )
    .await
}

/// [`connect`] with an explicit retry budget. A refusal the runtime marks
/// retryable with `retryAfterMs` is retried after that delay until `budget`
/// is spent. A refusal that remains is returned as the error; on an MCP
/// connection the host's `initialize` request, if it has already arrived, is
/// first answered with a JSON-RPC error carrying the same reason.
pub async fn connect_within<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    origin: &str,
    protocol: &str,
    bearer: &str,
    mut reader: R,
    mut writer: W,
    budget: std::time::Duration,
) -> Result<()> {
    let mut url = url::Url::parse(origin)?;
    if !matches!(protocol, "mcp" | "acp")
        || url.scheme() != "http"
        || !url
            .host_str()
            .and_then(|host| host.parse::<std::net::IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback())
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("shared gateway requires a loopback HTTP origin and mcp or acp protocol");
    }
    url.set_scheme("ws")
        .map_err(|_| anyhow::anyhow!("invalid gateway origin"))?;
    url.set_path(&format!("/v1/gateway/{protocol}"));
    let mut request = url.as_str().into_client_request()?;
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {bearer}"))?,
    );
    if protocol == "mcp" {
        if let Ok(value) = std::env::var("BOBBY_MCP_TOOLSET") {
            let value = value.trim();
            if matches!(value, "full" | "explore" | "act" | "intent" | "verify") {
                request
                    .headers_mut()
                    .insert("x-bobby-mcp-toolset", HeaderValue::from_str(value)?);
            }
        }
    }
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME_BYTES))
        .max_frame_size(Some(MAX_FRAME_BYTES));
    let started = tokio::time::Instant::now();
    let socket = loop {
        match tokio_tungstenite::connect_async_with_config(request.clone(), Some(config), false)
            .await
        {
            Ok((socket, _)) => break socket,
            Err(error) => {
                let refusal = refused(error);
                if let Some(delay) = refusal.retry_after.map(|delay| delay.max(MIN_RETRY_DELAY)) {
                    if started.elapsed() + delay <= budget {
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                }
                if refusal.runtime_answered && protocol == "mcp" {
                    answer_initialize(&mut reader, &mut writer, &refusal.error.to_string()).await;
                }
                return Err(refusal.error);
            }
        }
    };
    let (sink, stream) = socket.split();
    let incoming = stream.filter_map(async |message| match message {
        Ok(Message::Text(text)) => Some(Ok(text.to_string())),
        Ok(Message::Close(_)) => None,
        Ok(Message::Binary(_)) => Some(Err(tokio_tungstenite::tungstenite::Error::Io(
            std::io::Error::new(std::io::ErrorKind::InvalidData, "binary gateway frame"),
        ))),
        Ok(_) => None,
        Err(error) => Some(Err(error)),
    });
    let outgoing = sink.with(async |frame: String| {
        Ok::<_, tokio_tungstenite::tungstenite::Error>(Message::Text(frame.into()))
    });
    bridge(reader, writer, Box::pin(incoming), Box::pin(outgoing)).await
}

/// The scope's running owner, from `<scope>/runtime/owner.json`, when its
/// loopback port accepts connections and `bearer` is the scope's bootstrap
/// credential. A gateway launched without `BOBBY_RUNTIME_URL` but holding
/// that credential (a host entry written before `bobby mcp-stdio`) attaches
/// here instead of opening the owner's store a second time; a gateway with
/// any other credential stays a standalone runtime.
pub fn live_owner_origin(scope_dir: &std::path::Path, bearer: &str) -> Option<String> {
    let bootstrap = std::fs::read_to_string(scope_dir.join("bootstrap.env")).ok()?;
    let scope_bearer = bootstrap.lines().find_map(|line| {
        line.strip_prefix("AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN=")
            .map(|value| value.trim().trim_matches('"'))
    })?;
    if scope_bearer.is_empty() || scope_bearer != bearer {
        return None;
    }
    let owner: serde_json::Value =
        serde_json::from_slice(&std::fs::read(scope_dir.join("runtime/owner.json")).ok()?).ok()?;
    let origin = owner["url"].as_str()?;
    let url = url::Url::parse(origin).ok()?;
    let host: std::net::IpAddr = url.host_str()?.parse().ok()?;
    if url.scheme() != "http" || !host.is_loopback() {
        return None;
    }
    let address = std::net::SocketAddr::new(host, url.port()?);
    std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(500)).ok()?;
    Some(origin.to_owned())
}

struct Refusal {
    error: anyhow::Error,
    /// Set when the runtime marked the refusal retryable and named a delay.
    retry_after: Option<std::time::Duration>,
    /// The runtime itself answered the upgrade, as opposed to no connection.
    runtime_answered: bool,
}

/// The runtime's own reason when it answers the upgrade with an error, so a
/// full connection quota reads as that and not as a bad credential.
fn refused(error: tokio_tungstenite::tungstenite::Error) -> Refusal {
    let tokio_tungstenite::tungstenite::Error::Http(response) = &error else {
        return Refusal {
            error: anyhow::anyhow!("could not connect to the shared runtime"),
            retry_after: None,
            runtime_answered: false,
        };
    };
    let body = response
        .body()
        .as_deref()
        .and_then(|body| serde_json::from_slice::<serde_json::Value>(body).ok());
    let retry_after = body.as_ref().and_then(|body| {
        let error = &body["error"];
        if error["retryable"].as_bool() != Some(true) {
            return None;
        }
        error["retryAfterMs"]
            .as_u64()
            .map(std::time::Duration::from_millis)
    });
    let reason = body.as_ref().and_then(|body| {
        let error = &body["error"];
        let mut reason = format!(
            "{}: {}",
            error["code"].as_str()?,
            error["message"].as_str()?
        );
        if let Some(milliseconds) = error["retryAfterMs"].as_u64() {
            reason.push_str(&format!(" (retry after {milliseconds} ms)"));
        }
        Some(reason)
    });
    let error = match reason {
        Some(reason) => anyhow::anyhow!("the shared runtime refused the connection: {reason}"),
        None => anyhow::anyhow!(
            "the shared runtime refused the connection with HTTP {}",
            response.status()
        ),
    };
    Refusal {
        error,
        retry_after,
        runtime_answered: true,
    }
}

/// Answers the host's `initialize` request, when it is already waiting on
/// `reader`, with a JSON-RPC error so the host sees why the gateway is
/// exiting instead of a closed pipe. Best effort: nothing is answered when
/// no `initialize` arrives.
async fn answer_initialize<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    reason: &str,
) {
    let find = async {
        let mut reader = BufReader::new(reader);
        while let Ok(Some(frame)) = read_frame(&mut reader).await {
            let Ok(message) = serde_json::from_str::<serde_json::Value>(&frame) else {
                continue;
            };
            if message["method"] == "initialize" && message.get("id").is_some() {
                return Some(message["id"].clone());
            }
        }
        None
    };
    let Ok(Some(id)) = tokio::time::timeout(INITIALIZE_READ_WINDOW, find).await else {
        return;
    };
    let reply = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": -32000, "message": reason},
    });
    let _ = writer.write_all(format!("{reply}\n").as_bytes()).await;
    let _ = writer.flush().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn oversized_unterminated_input_is_bounded() {
        let input = vec![b'x'; MAX_FRAME_BYTES + 2];
        assert!(read_frame(&mut BufReader::new(input.as_slice()))
            .await
            .is_err());
    }
    #[tokio::test]
    async fn a_refused_upgrade_reports_the_runtime_reason() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            let body = r#"{"error":{"code":"resourceExhausted","layer":"interface","message":"principal in-flight capacity exhausted","correlationId":"00000000-0000-4000-8000-000000000000","commandId":null,"retryable":true,"retryAfterMs":1000,"reconciliationRequired":false,"requiredCapability":null}}"#;
            let response = format!(
                "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\nretry-after: 1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let error = connect_within(
            &origin,
            "mcp",
            "private-test-bearer",
            tokio::io::empty(),
            tokio::io::sink(),
            std::time::Duration::ZERO,
        )
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(
            error,
            "the shared runtime refused the connection: resourceExhausted: \
             principal in-flight capacity exhausted (retry after 1000 ms)"
        );
        assert!(!error.contains("private-test-bearer"));
    }

    const REFUSAL_BODY: &str = r#"{"error":{"code":"resourceExhausted","layer":"interface","message":"principal in-flight capacity exhausted","correlationId":"00000000-0000-4000-8000-000000000000","commandId":null,"retryable":true,"retryAfterMs":20,"reconciliationRequired":false,"requiredCapability":null}}"#;
    const REFUSAL_REASON: &str = "the shared runtime refused the connection: resourceExhausted: \
        principal in-flight capacity exhausted (retry after 20 ms)";

    async fn refuse(stream: &mut tokio::net::TcpStream) {
        let mut request = [0u8; 4096];
        let _ = stream.read(&mut request).await.unwrap();
        let response = format!(
            "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\n\
             retry-after: 1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{REFUSAL_BODY}",
            REFUSAL_BODY.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }

    #[tokio::test]
    async fn a_retryable_refusal_is_retried_until_the_runtime_admits_the_connection() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                refuse(&mut stream).await;
            }
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            while socket.next().await.is_some() {}
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            connect(
                &origin,
                "mcp",
                "private-test-bearer",
                tokio::io::empty(),
                tokio::io::sink(),
            ),
        )
        .await
        .expect("connect finished")
        .expect("the third attempt is admitted");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_persistent_refusal_answers_the_hosts_initialize_with_the_reason() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                refuse(&mut stream).await;
            }
        });
        let input =
            b"{\"jsonrpc\":\"2.0\",\"id\":\"init-7\",\"method\":\"initialize\",\"params\":{}}\n";
        let mut stdout = Vec::new();
        let started = std::time::Instant::now();
        let error = connect_within(
            &origin,
            "mcp",
            "private-test-bearer",
            input.as_slice(),
            &mut stdout,
            std::time::Duration::from_millis(200),
        )
        .await
        .unwrap_err()
        .to_string();
        server.abort();
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(error, REFUSAL_REASON);
        let reply: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(
            reply,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": "init-7",
                "error": {"code": -32000, "message": REFUSAL_REASON},
            })
        );
        let printed = String::from_utf8(stdout).unwrap();
        assert!(!printed.contains("private-test-bearer"));
        assert!(!error.contains("private-test-bearer"));
    }

    #[tokio::test]
    async fn a_refusal_without_a_pending_initialize_writes_nothing() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            refuse(&mut stream).await;
        });
        let mut stdout = Vec::new();
        let _ = connect_within(
            &origin,
            "mcp",
            "private-test-bearer",
            tokio::io::empty(),
            &mut stdout,
            std::time::Duration::ZERO,
        )
        .await
        .unwrap_err();
        assert!(stdout.is_empty());
    }

    #[test]
    fn a_live_scope_owner_is_found_and_a_dead_or_remote_one_is_not() {
        let scope = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(scope.path().join("runtime")).unwrap();
        std::fs::write(
            scope.path().join("bootstrap.env"),
            "AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN=\"scope-bearer\"\n",
        )
        .unwrap();
        let write = |url: &str| {
            std::fs::write(
                scope.path().join("runtime/owner.json"),
                serde_json::json!({"url": url}).to_string(),
            )
            .unwrap()
        };
        assert_eq!(live_owner_origin(scope.path(), "scope-bearer"), None);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live = format!("http://{}", listener.local_addr().unwrap());
        write(&live);
        assert_eq!(
            live_owner_origin(scope.path(), "scope-bearer"),
            Some(live.clone())
        );
        assert_eq!(
            live_owner_origin(scope.path(), "another-bearer"),
            None,
            "a gateway with its own credential must stay standalone"
        );
        drop(listener);
        let dead = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", probe.local_addr().unwrap())
        };
        write(&dead);
        assert_eq!(live_owner_origin(scope.path(), "scope-bearer"), None);
        write("http://203.0.113.7:80");
        assert_eq!(live_owner_origin(scope.path(), "scope-bearer"), None);
    }

    #[tokio::test]
    async fn remote_origins_are_rejected_before_sending_credentials() {
        assert!(connect(
            "http://example.com",
            "mcp",
            "private-test-bearer",
            tokio::io::empty(),
            tokio::io::sink()
        )
        .await
        .is_err());
    }
}
