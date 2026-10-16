//! Cookie option support through the production MCP runtime and native engines.
mod support;

use axum::{
    http::{header::SET_COOKIE, HeaderMap, HeaderValue},
    response::Html,
    routing::get,
    Router,
};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use support::rig::{Live, Rig};

fn cookie_names(outcome: &Value) -> BTreeSet<String> {
    assert_eq!(outcome["status"], "completed", "{outcome}");
    outcome["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["kind"] == "cookieState")
        .unwrap()["cookies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|cookie| cookie["name"].as_str().unwrap().to_owned())
        .collect()
}

async fn cookie_contract(rig: Rig, firefox: bool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/",
        get(|| async {
            let mut headers = HeaderMap::new();
            headers.append(
                SET_COOKIE,
                HeaderValue::from_static("normal=public; Path=/"),
            );
            headers.append(
                SET_COOKIE,
                HeaderValue::from_static("session=private; HttpOnly; Path=/"),
            );
            (
                headers,
                Html("<!doctype html><title>Cookies</title><main>Cookie contract</main>"),
            )
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let info = rig.tool("runtime_info", json!({})).await;
    let capabilities = info["capabilities"].as_array().unwrap();
    assert_eq!(
        capabilities.contains(&json!("engine:firefox:captureScreenshot.viewport")),
        firefox
    );
    assert_eq!(
        capabilities.contains(&json!("engine:chromium:deleteCookies.httpOnly")),
        !firefox
    );
    let live = Live::open(&rig, &url).await;
    let before = live.call("cookie_get", json!({"urls":[url]})).await;
    assert_eq!(
        cookie_names(&before),
        BTreeSet::from(["normal".into(), "session".into()])
    );
    assert!(before["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["kind"] == "cookieState"
            && item["cookies"]
                .as_array()
                .unwrap()
                .iter()
                .any(|cookie| cookie["name"] == "session" && cookie["httpOnly"] == true)));
    let deletion = live
        .call(
            "cookie_delete",
            json!({"urls":[url], "names":["normal","session"]}),
        )
        .await;
    if firefox {
        assert_eq!(deletion["status"], "failed", "{deletion}");
        assert_eq!(deletion["error"]["code"], "invalidRequest");
        assert_eq!(deletion["error"]["retryable"], false);
        let after = live.call("cookie_get", json!({"urls":[url]})).await;
        assert_eq!(
            cookie_names(&after),
            cookie_names(&before),
            "refusal must preserve every selected cookie"
        );
        let ordinary = live
            .call("cookie_delete", json!({"urls":[url],"names":["normal"]}))
            .await;
        assert_eq!(ordinary["status"], "completed", "{ordinary}");
        let after = live.call("cookie_get", json!({"urls":[url]})).await;
        assert_eq!(cookie_names(&after), BTreeSet::from(["session".into()]));
    } else {
        assert_eq!(deletion["status"], "completed", "{deletion}");
        let after = live.call("cookie_get", json!({"urls":[url]})).await;
        assert!(cookie_names(&after).is_empty());
    }
    live.close().await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium"]
async fn chromium_cookie_support_matches_native_behavior() {
    cookie_contract(Rig::chromium().await, false).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Firefox and companion test host"]
async fn firefox_cookie_support_refuses_before_partial_mutation() {
    cookie_contract(Rig::firefox().await, true).await;
}
