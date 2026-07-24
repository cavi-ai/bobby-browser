use std::io::Write;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::http::{header, HeaderValue};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use tokio::task::JoinHandle;

const INDEX: &str = r#"<!doctype html>
<html>
  <head><title>Runtime Fixture</title></head>
  <body>
    <main id="app">
      <label for="name">Name</label>
      <input id="name" autocomplete="off">
      <button id="continue" type="button">Continue</button>
      <label for="resume">Resume</label>
      <input id="resume" type="file">
    </main>
    <a id="root-popup" href="/popup" target="_blank">Open details</a>
    <a id="details-link" href="/details">Details</a>
    <iframe name="download-frame" hidden></iframe>
    <a id="download" href="/download" target="download-frame">Download fixture</a>
    <script>
      document.querySelector('#continue').addEventListener('click', () => {
        setTimeout(() => {
          document.querySelector('#app').insertAdjacentHTML('beforeend', `
            <section id="step-two">
              <label for="company">Company</label>
              <input id="company" autocomplete="off">
              <button id="submit" type="button">Submit</button>
              <a id="popup" href="/popup" target="_blank">Open details</a>
            </section>
          `);
          document.querySelector('#submit').addEventListener('click', () => {
            const name = document.querySelector('#name').value;
            const company = document.querySelector('#company').value;
            history.pushState({}, '', '/complete');
            document.querySelector('#app').innerHTML =
              `<div id="result">Submitted: ${name} @ ${company}</div>`;
          });
        }, 50);
      });
    </script>
  </body>
</html>"#;

pub struct FixtureServer {
    address: SocketAddr,
    task: JoinHandle<()>,
    peak_requests: Arc<AtomicUsize>,
}

impl FixtureServer {
    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn peak_requests(&self) -> usize {
        self.peak_requests.load(Ordering::SeqCst)
    }

    pub fn reset_peak_requests(&self) {
        self.peak_requests.store(0, Ordering::SeqCst);
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn spawn() -> FixtureServer {
    let drift_requests = Arc::new(AtomicUsize::new(0));
    let active_requests = Arc::new(AtomicUsize::new(0));
    let peak_requests = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .route("/complete", get(|| async { Html(INDEX) }))
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/static",
            get(|| async {
                let mut response = Html(
                    "<!doctype html><title>Static Fixture</title><p id='message' role='status'>café fixture</p>",
                )
                .into_response();
                response.headers_mut().append(
                    header::SET_COOKIE,
                    HeaderValue::from_static("fixture=fresh; Path=/; HttpOnly"),
                );
                response
                    .headers_mut()
                    .insert(header::ETAG, HeaderValue::from_static("\"fixture-v1\""));
                response
            }),
        )
        .route(
            "/redirect-static",
            get(|| async {
                (
                    axum::http::StatusCode::FOUND,
                    [(header::LOCATION, "/static")],
                )
            }),
        )
        .route(
            "/redirect-private",
            get(|| async {
                (
                    axum::http::StatusCode::FOUND,
                    [(header::LOCATION, "http://10.0.0.1/private")],
                )
            }),
        )
        .route("/gzip", get(|| async { compressed_response("gzip") }))
        .route(
            "/gzip-bomb",
            get(|| async { compressed_response_body("gzip", &vec![b'x'; 4096]) }),
        )
        .route("/brotli", get(|| async { compressed_response("br") }))
        .route(
            "/latin1",
            get(|| async {
                let mut response = b"<!doctype html><title>Latin</title><p>caf\xe9 fixture</p>"
                    .to_vec()
                    .into_response();
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/html; charset=iso-8859-1"),
                );
                response
            }),
        )
        .route(
            "/js-shell",
            get(|| async {
                Html("<!doctype html><div id='app'></div><script>document.querySelector('#app').innerHTML='<p id=dynamic>rendered fixture</p>'</script>")
            }),
        )
        .route(
            "/misleading",
            get(|| async {
                let mut response =
                    Html("<title>Actually HTML</title><p>not trustworthy</p>").into_response();
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/octet-stream"),
                );
                response
            }),
        )
        .route(
            "/oversized",
            get(|| async { Html("<p>012345678901234567890123456789</p>") }),
        )
        .route(
            "/interrupted",
            get(|| async {
                let stream = futures_util::stream::iter([
                    Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"short")),
                    Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "controlled fixture interruption",
                    )),
                ]);
                axum::response::Response::builder()
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .body(axum::body::Body::from_stream(stream))
                    .expect("interrupted fixture response")
            }),
        )
        .route(
            "/download",
            get(|| async {
                let mut response = b"workflow-download-v1".to_vec().into_response();
                response.headers_mut().append(
                    header::SET_COOKIE,
                    HeaderValue::from_static("downloaded=yes; Path=/; HttpOnly"),
                );
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/octet-stream"),
                );
                response.headers_mut().insert(
                    header::CONTENT_DISPOSITION,
                    HeaderValue::from_static("attachment; filename=workflow-fixture.bin"),
                );
                response
            }),
        )
        .route(
            "/download-secret-cookie",
            get(|| async {
                let mut response = b"secret-cookie-download".to_vec().into_response();
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/octet-stream"),
                );
                response.headers_mut().append(
                    header::SET_COOKIE,
                    HeaderValue::from_static(
                        "bad=super-secret-capability; Domain=com; Path=/; HttpOnly",
                    ),
                );
                response
            }),
        )
        .route(
            "/download-traversal",
            get(|| async { download_named("../../etc/passwd") }),
        )
        .route(
            "/download-control",
            get(|| async {
                let mut response = b"control".to_vec().into_response();
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/octet-stream"),
                );
                response.headers_mut().insert(
                    header::CONTENT_DISPOSITION,
                    HeaderValue::from_static("attachment; filename*=UTF-8''bad%07name.txt"),
                );
                response
            }),
        )
        .route(
            "/download-star",
            get(|| async {
                let mut response = b"star".to_vec().into_response();
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/octet-stream"),
                );
                response.headers_mut().insert(
                    header::CONTENT_DISPOSITION,
                    HeaderValue::from_static("attachment; filename*=UTF-8''caf%C3%A9.txt"),
                );
                response
            }),
        )
        .route(
            "/slow",
            get({
                let active = active_requests.clone();
                let peak = peak_requests.clone();
                move || {
                    let active = active.clone();
                    let peak = peak.clone();
                    async move {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        Html("<title>Slow</title><p id='slow-proof'>slow fixture</p>")
                    }
                }
            }),
        )
        .route(
            "/slow-redirect",
            get(|| async {
                tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                (axum::http::StatusCode::FOUND, [(header::LOCATION, "/slow")])
            }),
        )
        .route(
            "/cookie-echo",
            get(|headers: axum::http::HeaderMap| async move {
                Html(format!(
                    "<title>Cookies</title><p role='status'>{}</p>",
                    headers
                        .get(header::COOKIE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("none")
                ))
            }),
        )
        .route(
            "/cookie-start",
            get(|| async {
                let mut response = (
                    axum::http::StatusCode::FOUND,
                    [(header::LOCATION, "/cookie-replace")],
                )
                    .into_response();
                response.headers_mut().insert(
                    header::SET_COOKIE,
                    HeaderValue::from_static("session=old; Path=/"),
                );
                response
            }),
        )
        .route(
            "/cookie-replace",
            get(|| async {
                let mut response = (
                    axum::http::StatusCode::FOUND,
                    [(header::LOCATION, "/cookie-echo")],
                )
                    .into_response();
                response.headers_mut().insert(
                    header::SET_COOKIE,
                    HeaderValue::from_static("session=new; Path=/"),
                );
                response
            }),
        )
        .route(
            "/cookie-domain-public",
            get(|| async {
                let mut response = (
                    axum::http::StatusCode::FOUND,
                    [(header::LOCATION, "/cookie-echo")],
                )
                    .into_response();
                response.headers_mut().insert(
                    header::SET_COOKIE,
                    HeaderValue::from_static("bad=public; Domain=com; Path=/"),
                );
                response
            }),
        )
        .route(
            "/cookie-domain-super",
            get(|| async {
                let mut response = (
                    axum::http::StatusCode::FOUND,
                    [(header::LOCATION, "/cookie-echo")],
                )
                    .into_response();
                response.headers_mut().insert(
                    header::SET_COOKIE,
                    HeaderValue::from_static("bad=super; Domain=0.0.1; Path=/"),
                );
                response
            }),
        )
        .route(
            "/validator",
            get(|headers: axum::http::HeaderMap| async move {
                if headers
                    .get(header::IF_NONE_MATCH)
                    .and_then(|v| v.to_str().ok())
                    == Some("\"fixture-v1\"")
                {
                    axum::http::StatusCode::NOT_MODIFIED.into_response()
                } else {
                    let mut response = Html("<title>Validator</title><p>fresh</p>").into_response();
                    response
                        .headers_mut()
                        .insert(header::ETAG, HeaderValue::from_static("\"fixture-v1\""));
                    response
                }
            }),
        )
        .route(
            "/no-content",
            get(|| async { axum::http::StatusCode::NO_CONTENT }),
        )
        .route(
            "/download-colon",
            get(|| async { download_named("C:secret.txt") }),
        )
        .route("/download-con", get(|| async { download_named("CON.txt") }))
        .route(
            "/download-lpt",
            get(|| async { download_named("lPt9.log") }),
        )
        .route(
            "/download-trailing",
            get(|| async { download_named("report. ") }),
        )
        .route(
            "/popup",
            get(|| async { Html("<title>Popup</title><p id='details'>Details</p>") }),
        )
        .route(
            "/details",
            get(|| async { Html("<!doctype html><title>Details</title><p id='details-page'>Details page</p>") }),
        )
        .route(
            "/obstructed",
            get(|| async {
                Html(
                    r#"<!doctype html><title>Obstructed</title>
                    <div id="page-content"><button id="primary-action" type="button">Primary action</button></div>
                    <div id="cookie-banner" role="dialog" aria-label="Cookie notice">
                      <p>We use cookies.</p>
                      <button id="cookie-close" aria-label="Close cookie notice" type="button">Close</button>
                    </div>
                    <script>
                      document.querySelector('#cookie-close').addEventListener('click', () => {
                        document.querySelector('#cookie-banner').remove();
                      });
                    </script>"#,
                )
            }),
        )
        .route(
            "/obstructed-stuck",
            get(|| async {
                Html(
                    r#"<!doctype html><title>Obstructed Stuck</title>
                    <div id="cookie-banner" role="dialog" aria-label="Cookie notice">
                      <p>We use cookies.</p>
                      <button id="cookie-close" aria-label="Close cookie notice" type="button">Close</button>
                    </div>
                    <script>
                      document.querySelector('#cookie-close').addEventListener('click', () => {});
                    </script>"#,
                )
            }),
        )
        .route(
            "/profile",
            get(|| async {
                Html(
                    r#"<!doctype html><title>Profile</title>
                    <h1 id="display-name" role="heading" data-user-id="42">Ada Lovelace</h1>
                    <a id="profile-link" href="/profile/42">View profile</a>"#,
                )
            }),
        )
        .route(
            "/drift",
            get({
                let drift_requests = drift_requests.clone();
                move || {
                    let drift_requests = drift_requests.clone();
                    async move {
                        let title = if drift_requests.fetch_add(1, Ordering::SeqCst) == 0 {
                            "Stable Checkpoint"
                        } else {
                            "Drifted State"
                        };
                        Html(format!("<title>{title}</title><p id='state'>{title}</p>"))
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture listener");
    let address = listener.local_addr().expect("read fixture address");
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve deterministic fixture");
    });
    FixtureServer {
        address,
        task,
        peak_requests,
    }
}

fn compressed_response(encoding: &str) -> axum::response::Response {
    let input = b"<!doctype html><title>Compressed</title><p>compressed fixture</p>";
    compressed_response_body(encoding, input)
}

fn compressed_response_body(encoding: &str, input: &[u8]) -> axum::response::Response {
    let bytes = if encoding == "gzip" {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(input).expect("compress gzip fixture");
        encoder.finish().expect("finish gzip fixture")
    } else {
        let mut bytes = Vec::new();
        {
            let mut encoder = brotli::CompressorWriter::new(&mut bytes, 4096, 5, 22);
            encoder.write_all(input).expect("compress brotli fixture");
        }
        bytes
    };
    let mut response = bytes.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::CONTENT_ENCODING,
        HeaderValue::from_str(encoding).expect("fixture encoding"),
    );
    response
}

fn download_named(filename: &'static str) -> axum::response::Response {
    let mut response = b"named".to_vec().into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .expect("fixture filename"),
    );
    response
}

pub async fn spawn_frame_host(child_url: &str) -> FixtureServer {
    let child_url = child_url.to_string();
    let app = Router::new().route(
        "/",
        get(move || {
            let child_url = child_url.clone();
            async move {
                Html(format!(
                    r#"<!doctype html><title>Frame Host</title>
                    <iframe name="outer" aria-label="Outer" srcdoc='<iframe name="fixture" aria-label="Cross" src="{child_url}"></iframe>'></iframe>
                    <div id="host"></div>
                    <button id="old-action" aria-label="Drift action" onclick="this.outerHTML='<button aria-label=&quot;Drift action&quot; onclick=&quot;document.querySelector(\'#status\').textContent=\'ready\'&quot;>replacement</button>'">initial</button>
                    <p id="status">waiting</p>
                    <button aria-label="Ambiguous">one</button><button aria-label="Ambiguous">two</button>
                    <script>
                      const root = host.attachShadow({{mode:'open'}});
                      root.innerHTML = `<button aria-label="Inside" onclick="document.title='shadow-clicked'">inside</button>`;
                    </script>"#
                ))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind frame host fixture listener");
    let address = listener.local_addr().expect("read frame host address");
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve frame host fixture");
    });
    FixtureServer {
        address,
        task,
        peak_requests: Arc::new(AtomicUsize::new(0)),
    }
}
