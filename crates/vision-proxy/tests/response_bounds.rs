use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vision_proxy::{
    ExtractInput, MlxUpstream, OllamaUpstream, OpenAiUpstream, ProposeInput, Upstream,
    UpstreamError,
};

const LIMIT: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug)]
enum Provider {
    OpenAi,
    Ollama,
    Mlx,
}

impl Provider {
    fn upstream(self, base: String) -> Box<dyn Upstream> {
        match self {
            Self::OpenAi => Box::new(OpenAiUpstream::new(String::new(), "model".into(), base)),
            Self::Ollama => Box::new(OllamaUpstream::new("model".into(), base)),
            Self::Mlx => Box::new(MlxUpstream::new(base)),
        }
    }

    fn body(self, extract: bool) -> Vec<u8> {
        let value = if extract {
            json!({"value": {"title": "Example"}})
        } else {
            json!({"confidence": 0.9, "action": {"kind": "click", "x": 12.0, "y": 34.0}})
        };
        let envelope = match self {
            Self::Mlx => value,
            _ => json!({"choices": [{"message": {"content": value.to_string()}}]}),
        };
        serde_json::to_vec(&envelope).unwrap()
    }
}

enum Framing {
    Chunked,
    Gzip,
    HeadersOnly,
}

struct Fixture {
    base: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fixture(status: u16, body: Vec<u8>, framing: Framing) -> Fixture {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let count = socket.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0);
            request.extend_from_slice(&buffer[..count]);
            if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        let framing_header = match framing {
            Framing::Chunked => "Transfer-Encoding: chunked\r\n".to_owned(),
            Framing::Gzip => format!(
                "Content-Encoding: gzip\r\nContent-Length: {}\r\n",
                body.len()
            ),
            Framing::HeadersOnly => format!("Content-Length: {}\r\n", LIMIT + 1),
        };
        socket
            .write_all(
                format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\n{framing_header}Connection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        match framing {
            Framing::HeadersOnly => {
                // A rejected response must be dropped without waiting for its body.
                assert_eq!(socket.read(&mut buffer).await.unwrap(), 0);
            }
            Framing::Gzip => {
                let _ = socket.write_all(&body).await;
            }
            Framing::Chunked => {
                for chunk in body.chunks(4096) {
                    if socket
                        .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                        .await
                        .is_err()
                        || socket.write_all(chunk).await.is_err()
                        || socket.write_all(b"\r\n").await.is_err()
                    {
                        return;
                    }
                }
                let _ = socket.write_all(b"0\r\n\r\n").await;
            }
        }
    });
    Fixture {
        base: format!("http://{address}"),
        task,
    }
}

async fn invoke(upstream: &dyn Upstream, extract: bool) -> Result<Value, UpstreamError> {
    if extract {
        upstream
            .extract(ExtractInput {
                schema: json!({"type": "object"}),
                content: "Example".into(),
                purpose: None,
            })
            .await
            .map(|response| response.value)
    } else {
        upstream
            .propose(ProposeInput {
                purpose: "Continue".into(),
                intent_kind: "locate".into(),
                stuck: "targetMissing".into(),
                screenshot_png_b64: "png".into(),
                corpus_screenshot_png_b64: None,
                context: None,
            })
            .await
            .map(|response| json!({"confidence": response.confidence}))
    }
}

async fn oversized_chunked_body(provider: Provider) {
    for extract in [false, true] {
        let mut body = provider.body(extract);
        body.resize(LIMIT + 1, b' ');
        let fixture = fixture(200, body, Framing::Chunked).await;
        let upstream = provider.upstream(fixture.base.clone());
        let error = invoke(upstream.as_ref(), extract).await.unwrap_err();
        assert!(matches!(error, UpstreamError::Invalid(_)), "{error}");
        assert!(error.to_string().contains("exceeded"), "{error}");
    }
}

async fn declared_oversize(provider: Provider) {
    let mut fixture = fixture(200, Vec::new(), Framing::HeadersOnly).await;
    let upstream = provider.upstream(fixture.base.clone());
    let result = tokio::time::timeout(Duration::from_secs(2), invoke(upstream.as_ref(), false))
        .await
        .expect("must reject declared oversize before waiting for body");
    let error = result.unwrap_err();
    assert!(matches!(error, UpstreamError::Invalid(_)), "{error}");
    assert!(error.to_string().contains("exceeded"), "{error}");
    tokio::time::timeout(Duration::from_secs(2), &mut fixture.task)
        .await
        .expect("oversized response connection must close")
        .unwrap();
}

async fn rejection_does_not_wait_for_body(provider: Provider) {
    let fixture = fixture(403, Vec::new(), Framing::HeadersOnly).await;
    let upstream = provider.upstream(fixture.base.clone());
    let error = tokio::time::timeout(Duration::from_secs(2), invoke(upstream.as_ref(), false))
        .await
        .expect("rejection must not read or wait for response body")
        .unwrap_err();
    assert!(matches!(error, UpstreamError::Rejected(_)), "{error}");
    assert!(error.to_string().contains("403"), "{error}");
}

async fn rejection_hides_body(provider: Provider) {
    let fixture = fixture(500, b"UPSTREAM_SECRET_BODY".to_vec(), Framing::Chunked).await;
    let upstream = provider.upstream(fixture.base.clone());
    let error = invoke(upstream.as_ref(), false).await.unwrap_err();
    assert!(matches!(error, UpstreamError::Rejected(_)), "{error}");
    assert!(error.to_string().contains("500"), "{error}");
    assert!(
        !error.to_string().contains("UPSTREAM_SECRET_BODY"),
        "{error}"
    );
}

async fn accepts_exact_limit(provider: Provider) {
    for extract in [false, true] {
        let mut body = provider.body(extract);
        body.resize(LIMIT, b' ');
        let fixture = fixture(200, body, Framing::Chunked).await;
        let upstream = provider.upstream(fixture.base.clone());
        let value = invoke(upstream.as_ref(), extract).await.unwrap();
        if extract {
            assert_eq!(value["title"], "Example");
        } else {
            assert!((value["confidence"].as_f64().unwrap() - 0.9).abs() < 1e-6);
        }
    }
}

async fn decoded_gzip_oversize(provider: Provider) {
    // gzip.compress(b' ' * (1024 * 1024 + 1), mtime=0): small wire body,
    // oversized decoded body. No JSON parsing should occur.
    let body = include_bytes!("fixtures/oversized_whitespace.json.gz").to_vec();
    assert!(body.len() < LIMIT);
    let fixture = fixture(200, body, Framing::Gzip).await;
    let upstream = provider.upstream(fixture.base.clone());
    let error = invoke(upstream.as_ref(), false).await.unwrap_err();
    assert!(matches!(error, UpstreamError::Invalid(_)), "{error}");
    assert!(error.to_string().contains("exceeded"), "{error}");
}

macro_rules! provider_tests {
    ($name:ident, $provider:expr) => {
        mod $name {
            use super::*;
            #[tokio::test]
            async fn rejects_oversized_chunked_proposal_and_extract() {
                oversized_chunked_body($provider).await;
            }
            #[tokio::test]
            async fn rejects_declared_oversize_without_reading_body() {
                declared_oversize($provider).await;
            }
            #[tokio::test]
            async fn rejection_does_not_read_body() {
                rejection_does_not_wait_for_body($provider).await;
            }
            #[tokio::test]
            async fn rejection_does_not_expose_body() {
                rejection_hides_body($provider).await;
            }
            #[tokio::test]
            async fn accepts_proposal_and_extract_at_exact_limit() {
                accepts_exact_limit($provider).await;
            }
            #[tokio::test]
            async fn rejects_decoded_gzip_oversize() {
                decoded_gzip_oversize($provider).await;
            }
        }
    };
}

provider_tests!(openai, Provider::OpenAi);
provider_tests!(ollama, Provider::Ollama);
provider_tests!(mlx, Provider::Mlx);
