use std::net::SocketAddr;

use axum::response::Html;
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
    </main>
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
    let app = Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/popup",
            get(|| async { Html("<title>Popup</title><p id='details'>Details</p>") }),
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
