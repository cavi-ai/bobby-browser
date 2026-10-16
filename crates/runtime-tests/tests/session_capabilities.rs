//! Session discovery agrees with selected native worker and dispatch policy.
mod support;

use axum::{response::Html, routing::get, Router};
use serde_json::{json, Value};
use support::rig::{Live, Rig};

fn feature<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["features"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == name)
        .unwrap()
}

async fn contract(rig: Rig, engine: &str) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/",
                get(|| async {
                    Html("<!doctype html><title>Capabilities</title><h1>Native contract</h1>")
                }),
            ),
        )
        .await
        .unwrap();
    });
    let live = Live::open(&rig, &url).await;
    let report = rig
        .tool("session_capabilities", json!({"sessionId":live.session_id}))
        .await;
    assert_eq!(report["engine"], engine, "{report}");
    assert_eq!(report["connection"], "connected", "{report}");
    assert_eq!(
        feature(&report, "captureScreenshot.viewport")["available"],
        true
    );
    assert_eq!(
        feature(&report, "captureScreenshot.fullPage")["supported"],
        engine == "chromium"
    );
    assert_eq!(
        feature(&report, "evaluateJavaScript")["reason"],
        "policyDisabled"
    );
    let screenshot = live.call("screenshot", json!({})).await;
    assert_eq!(screenshot["status"], "completed", "{screenshot}");
    live.close().await;
    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium"]
async fn chromium_session_capabilities_match_worker() {
    contract(Rig::chromium().await, "chromium").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Firefox and companion test host"]
async fn firefox_session_capabilities_match_worker() {
    contract(Rig::firefox().await, "firefox").await;
}
