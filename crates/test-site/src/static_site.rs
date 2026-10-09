//! Minimal local fixture server for regression tests: static HTML routes
//! defined by the test, and real HTTP 302 redirects.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
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
    /// Serves `body` with this content type after `delay`.
    Delayed {
        delay: std::time::Duration,
        content_type: &'static str,
        body: String,
    },
    /// Serves `body` with this content type after the request's `ms` query
    /// parameter in milliseconds, at most ten seconds.
    QueryDelayed {
        content_type: &'static str,
        body: String,
    },
}

type Bodies = Arc<Mutex<Vec<Vec<u8>>>>;

struct Entry {
    route: Route,
    hits: Arc<AtomicUsize>,
    bodies: Bodies,
}

pub struct FixtureSite {
    address: SocketAddr,
    task: JoinHandle<()>,
    hits: HashMap<String, Arc<AtomicUsize>>,
    bodies: HashMap<String, Bodies>,
}

impl FixtureSite {
    pub async fn spawn(routes: Vec<(&str, Route)>) -> Self {
        let hits: HashMap<String, Arc<AtomicUsize>> = routes
            .iter()
            .map(|(path, _)| ((*path).to_owned(), Arc::new(AtomicUsize::new(0))))
            .collect();
        let bodies: HashMap<String, Bodies> = routes
            .iter()
            .map(|(path, _)| ((*path).to_owned(), Bodies::default()))
            .collect();
        let table: HashMap<String, Entry> = routes
            .into_iter()
            .map(|(path, route)| {
                let counter = Arc::clone(&hits[path]);
                let captured = Arc::clone(&bodies[path]);
                (
                    path.to_owned(),
                    Entry {
                        route,
                        hits: counter,
                        bodies: captured,
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
            bodies,
        }
    }

    /// Request bodies the route at `path` has received, in arrival order
    /// (an empty body is recorded too, so the count matches `hits`).
    pub fn bodies(&self, path: &str) -> Vec<Vec<u8>> {
        self.bodies
            .get(path)
            .map(|bodies| bodies.lock().expect("fixture bodies lock").clone())
            .unwrap_or_default()
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

async fn serve(
    State(table): State<Arc<HashMap<String, Entry>>>,
    uri: Uri,
    body: Bytes,
) -> Response {
    let Some(entry) = table.get(uri.path()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    entry
        .bodies
        .lock()
        .expect("fixture bodies lock")
        .push(body.to_vec());
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
        Route::Delayed {
            delay,
            content_type,
            body,
        } => {
            tokio::time::sleep(*delay).await;
            ([(header::CONTENT_TYPE, *content_type)], body.clone()).into_response()
        }
        Route::QueryDelayed { content_type, body } => {
            let ms = uri
                .query()
                .into_iter()
                .flat_map(|query| query.split('&'))
                .find_map(|pair| pair.strip_prefix("ms="))
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0)
                .min(10_000);
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            ([(header::CONTENT_TYPE, *content_type)], body.clone()).into_response()
        }
    }
}
