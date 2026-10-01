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

pub async fn connect<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    origin: &str,
    protocol: &str,
    bearer: &str,
    reader: R,
    writer: W,
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
    let (socket, _) = tokio_tungstenite::connect_async_with_config(request, Some(config), false)
        .await
        .map_err(refused)?;
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

/// The runtime's own reason when it answers the upgrade with an error, so a
/// full connection quota reads as that and not as a bad credential.
fn refused(error: tokio_tungstenite::tungstenite::Error) -> anyhow::Error {
    let tokio_tungstenite::tungstenite::Error::Http(response) = &error else {
        return anyhow::anyhow!("could not connect to the shared runtime");
    };
    let reason = response
        .body()
        .as_deref()
        .and_then(|body| serde_json::from_slice::<serde_json::Value>(body).ok())
        .and_then(|body| {
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
    match reason {
        Some(reason) => anyhow::anyhow!("the shared runtime refused the connection: {reason}"),
        None => anyhow::anyhow!(
            "the shared runtime refused the connection with HTTP {}",
            response.status()
        ),
    }
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
        let error = connect(
            &origin,
            "mcp",
            "private-test-bearer",
            tokio::io::empty(),
            tokio::io::sink(),
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
