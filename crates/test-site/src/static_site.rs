//! Minimal local fixture server for regression tests: static HTML routes
//! defined by the test, and real HTTP 302 redirects.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use axum::Router;
use tokio::task::JoinHandle;

pub enum Route {
    /// Serves this document with status 200.
    Html(String),
    /// Always answers 302 with this `Location`.
    Redirect(String),
    /// Answers 302 to `to` on the first request, then serves `then`.
    RedirectOnce { to: String, then: String },
    /// Serves `body` with this content type. Any method is accepted.
    Raw {
        content_type: &'static str,
        body: String,
    },
}

struct Entry {
    route: Route,
    hits: Arc<AtomicUsize>,
}

pub struct FixtureSite {
    address: SocketAddr,
    task: JoinHandle<()>,
    hits: HashMap<String, Arc<AtomicUsize>>,
}

impl FixtureSite {
    pub async fn spawn(routes: Vec<(&str, Route)>) -> Self {
        let hits: HashMap<String, Arc<AtomicUsize>> = routes
            .iter()
            .map(|(path, _)| ((*path).to_owned(), Arc::new(AtomicUsize::new(0))))
            .collect();
        let table: HashMap<String, Entry> = routes
            .into_iter()
            .map(|(path, route)| {
                let counter = Arc::clone(&hits[path]);
                (
                    path.to_owned(),
                    Entry {
                        route,
                        hits: counter,
                    },
                )
            })
            .collect();
        let app = Router::new().fallback(serve).with_state(Arc::new(table));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture listener");
        let address = listener.local_addr().expect("read fixture address");
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve regression fixture");
        });
        Self {
            address,
            task,
            hits,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    /// Requests the route at `path` has served so far (any method).
    pub fn hits(&self, path: &str) -> usize {
        self.hits
            .get(path)
            .map_or(0, |counter| counter.load(Ordering::SeqCst))
    }
}

impl Drop for FixtureSite {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn redirect(to: &str) -> Response {
    (StatusCode::FOUND, [(header::LOCATION, to.to_owned())]).into_response()
}

async fn serve(State(table): State<Arc<HashMap<String, Entry>>>, uri: Uri) -> Response {
    let Some(entry) = table.get(uri.path()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let hit = entry.hits.fetch_add(1, Ordering::SeqCst);
    match &entry.route {
        Route::Html(body) => Html(body.clone()).into_response(),
        Route::Redirect(to) => redirect(to),
        Route::Raw { content_type, body } => {
            ([(header::CONTENT_TYPE, *content_type)], body.clone()).into_response()
        }
        Route::RedirectOnce { to, then } => {
            if hit == 0 {
                redirect(to)
            } else {
                Html(then.clone()).into_response()
            }
        }
    }
}
