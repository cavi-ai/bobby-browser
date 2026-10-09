//! One browser-policy contract through the production runtime on both engines.
mod support;

use serde_json::json;
use std::time::Duration;
use support::rig::{Live, Rig};
use test_site::{FixtureSite, Route};

enum Engine {
    Chromium,
    Firefox,
}

async fn quiet_window_contract(engine: Engine) {
    let site = FixtureSite::spawn(vec![
        ("/", Route::Html(
            "<!doctype html><title>Quiet window contract</title>\
             <label>Name<input aria-label='Name'></label>\
             <button onclick=\"fetch('/slow').then(() => document.querySelector('#status').textContent = 'Finished')\">Start request</button>\
             <p id='status'>Pending</p>".into(),
        )),
        ("/slow", Route::Delayed {
            delay: Duration::from_secs(1),
            content_type: "text/plain",
            body: "ok".into(),
        }),
    ]).await;
    let rig = match engine {
        Engine::Chromium => Rig::chromium().await,
        Engine::Firefox => Rig::firefox().await,
    };
    let live = Live::open(&rig, &site.url("/")).await;
    let fill = live
        .call(
            "intent_fill",
            json!({
                "purpose":"Name", "hints":{"accessibleName":"Name"},
                "value":{"kind":"setText", "value":"contract"}
            }),
        )
        .await;
    assert_eq!(fill["status"], "completed", "{fill}");
    let click = live.call("click", json!({"selector":"button"})).await;
    assert_eq!(click["status"], "completed", "{click}");
    let wait = live
        .call(
            "wait_for",
            json!({
                "condition":{"kind":"networkQuiet", "idleMs":100, "maxInFlight":0},
                "timeoutMs":5000
            }),
        )
        .await;
    assert_eq!(wait["status"], "completed", "{wait}");
    assert_eq!(site.hits("/slow"), 1);
    let inspection = live.call("inspect", json!({"selector":"#status"})).await;
    assert_eq!(inspection["status"], "completed", "{inspection}");
    assert!(
        inspection.to_string().contains("Finished"),
        "quiet returned before the request finished: {inspection}"
    );
    live.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chrome or Chromium"]
async fn quiet_window_chromium() {
    quiet_window_contract(Engine::Chromium).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Firefox and paired test profile"]
async fn quiet_window_firefox() {
    quiet_window_contract(Engine::Firefox).await;
}
