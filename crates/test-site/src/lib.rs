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
      <input id="resume" type="file">
    </main>
    <a id="root-popup" href="/popup" target="_blank">Open details</a>
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
}

impl FixtureServer {
    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn spawn() -> FixtureServer {
    let drift_requests = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/static",
            get(|| async {
                let mut response = Html(
                    "<!doctype html><title>Static Fixture</title><p id='message'>café fixture</p>",
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
            get(|| async { Html("<!doctype html><div id='app'></div><script>render()</script>") }),
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
                let mut response = b"short".to_vec().into_response();
                response
                    .headers_mut()
                    .insert(header::CONTENT_LENGTH, HeaderValue::from_static("100"));
                response
            }),
        )
        .route(
            "/download",
            get(|| async {
                let mut response = b"workflow-download-v1".to_vec().into_response();
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
            "/popup",
            get(|| async { Html("<title>Popup</title><p id='details'>Details</p>") }),
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
    FixtureServer { address, task }
}

fn compressed_response(encoding: &str) -> axum::response::Response {
    let input = b"<!doctype html><title>Compressed</title><p>compressed fixture</p>";
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
    FixtureServer { address, task }
}
