//! Shared browser-policy contracts through the production runtime on both engines.
mod support;

use serde_json::{json, Value};
use std::time::Duration;
use support::rig::{Live, Rig};
use test_site::{FixtureSite, Route};

enum Engine {
    Chromium,
    Firefox,
}

async fn quiet_window_contract(engine: Engine) {
    let uploads = tempfile::tempdir().unwrap();
    let upload_path = uploads.path().join("contract.txt");
    std::fs::write(&upload_path, "policy contract").unwrap();
    let site = FixtureSite::spawn(vec![
        ("/", Route::Html(
            "<!doctype html><title>Quiet window contract</title>\
             <label>Name<input aria-label='Name'></label>\
             <button onclick=\"fetch('/slow').then(() => document.querySelector('#status').textContent = 'Finished')\">Start request</button>\
             <p id='status'>Pending</p><input id='file' type='file'><iframe src='/frame'></iframe>".into(),
        )),
        ("/frame", Route::Html("<!doctype html><body>Frame ready</body>".into())),
        ("/slow", Route::Delayed {
            delay: Duration::from_secs(1),
            content_type: "text/plain",
            body: "ok".into(),
        }),
    ]).await;
    let rig = match engine {
        Engine::Chromium => Rig::chromium_with_upload_roots(vec![uploads.path().into()]).await,
        Engine::Firefox => Rig::firefox_with_upload_roots(vec![uploads.path().into()]).await,
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
    for condition in [
        json!({"kind":"url", "matcher":{"kind":"contains", "value":"/"}}),
        json!({"kind":"element", "target":{"css":"#status"}, "state":"visible"}),
        json!({"kind":"text", "target":{"css":"body"}, "matcher":{"kind":"contains", "value":"Finished"}}),
        json!({"kind":"value", "target":{"css":"input[aria-label='Name']"}, "matcher":{"kind":"exact", "value":"contract"}}),
        json!({"kind":"document", "ready":"interactive"}),
        json!({"kind":"text", "target":{"css":"body", "framePath":[{"css":"iframe"}]}, "matcher":{"kind":"contains", "value":"Frame ready"}}),
    ] {
        let outcome = live
            .call("wait_for", json!({"condition":condition, "timeoutMs":1000}))
            .await;
        assert_eq!(outcome["status"], "completed", "{outcome}");
        assert!(outcome.to_string().contains("observations"), "{outcome}");
    }
    let uploaded = live
        .call(
            "upload_files",
            json!({"selector":"#file", "paths":[upload_path]}),
        )
        .await;
    assert_eq!(uploaded["status"], "completed", "{uploaded}");
    assert!(
        uploaded.to_string().contains("upload://sha256/"),
        "{uploaded}"
    );
    assert!(
        !uploaded
            .to_string()
            .contains(uploads.path().to_str().unwrap()),
        "upload evidence disclosed a local path: {uploaded}"
    );
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

async fn element_visibility_contract(engine: Engine) {
    const CONTROLS: &str = "<button id='visible'>Visible</button>\
        <button id='display-none' style='display:none'>Hidden</button>\
        <button id='visibility-hidden' style='visibility:hidden'>Hidden</button>\
        <div id='zero-area' style='width:0;height:0;overflow:hidden'></div>";
    let site = FixtureSite::spawn(vec![
        ("/", Route::Html(format!(
            "<!doctype html><title>Element visibility contract</title>{CONTROLS}\
             <iframe id='frame' src='/frame'></iframe><div id='shadow-host'></div>\
             <script>document.querySelector('#shadow-host').attachShadow({{mode:'open'}}).innerHTML={};</script>",
            json!(CONTROLS),
        ))),
        ("/frame", Route::Html(format!("<!doctype html><title>Frame</title>{CONTROLS}"))),
    ])
    .await;
    let rig = match engine {
        Engine::Chromium => Rig::chromium().await,
        Engine::Firefox => Rig::firefox().await,
    };
    let live = Live::open(&rig, &site.url("/")).await;
    for scope in [
        json!({}),
        json!({"framePath":[{"css":"#frame"}]}),
        json!({"shadowPath":[{"css":"#shadow-host"}]}),
    ] {
        for selector in ["#display-none", "#visibility-hidden", "#zero-area"] {
            let mut target = scope.clone();
            target["css"] = json!(selector);
            for state in ["attached", "hidden"] {
                let condition = json!({"kind":"element", "target":target, "state":state});
                let result = live
                    .call("wait_for", json!({"condition":condition, "timeoutMs":1000}))
                    .await;
                assert_eq!(
                    result["status"], "completed",
                    "{scope} {selector} {state}: {result}"
                );
                let expected: types::WaitCondition = serde_json::from_value(condition).unwrap();
                assert_eq!(evidence(&result, "wait")["condition"], json!(expected));
            }
            let result = live
                .call(
                    "wait_for",
                    json!({"condition":{"kind":"element", "target":target, "state":"visible"}, "timeoutMs":200}),
                )
                .await;
            assert_eq!(
                result["status"], "failed",
                "hidden element reported visible: {result}"
            );
            assert_eq!(result["error"]["code"], "waitConditionTimedOut", "{result}");
        }
        let mut missing = scope.clone();
        missing["css"] = json!("#missing");
        for (state, status) in [("hidden", "completed"), ("visible", "failed")] {
            let result = live
                .call(
                    "wait_for",
                    json!({"condition":{"kind":"element", "target":missing, "state":state}, "timeoutMs":200}),
                )
                .await;
            assert_eq!(
                result["status"], status,
                "{scope} missing {state}: {result}"
            );
            if status == "failed" {
                assert_eq!(result["error"]["code"], "waitConditionTimedOut", "{result}");
            }
        }
        let mut target = scope;
        target["css"] = json!("#visible");
        let visible = live
            .call(
                "wait_for",
                json!({"condition":{"kind":"element", "target":target, "state":"visible"}, "timeoutMs":1000}),
            )
            .await;
        assert_eq!(visible["status"], "completed", "{visible}");
    }
    live.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chrome or Chromium"]
async fn element_visibility_chromium() {
    element_visibility_contract(Engine::Chromium).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Firefox and paired test profile"]
async fn element_visibility_firefox() {
    element_visibility_contract(Engine::Firefox).await;
}

async fn empty_wait_target_contract(engine: Engine) {
    const CONTROLS: &str = "<p id='empty-text' style='width:100px;height:20px'></p>\
        <input id='empty-value' value=''>";
    let site = FixtureSite::spawn(vec![
        ("/", Route::Html(format!(
            "<!doctype html><title>Empty wait target contract</title>{CONTROLS}\
             <iframe id='frame' src='/frame'></iframe><div id='shadow-host'></div>\
             <script>document.querySelector('#shadow-host').attachShadow({{mode:'open'}}).innerHTML={};</script>",
            json!(CONTROLS),
        ))),
        ("/frame", Route::Html(format!("<!doctype html><title>Frame</title>{CONTROLS}"))),
    ])
    .await;
    let rig = match engine {
        Engine::Chromium => Rig::chromium().await,
        Engine::Firefox => Rig::firefox().await,
    };
    let live = Live::open(&rig, &site.url("/")).await;
    let mut failures = Vec::new();
    for scope in [
        json!({}),
        json!({"framePath":[{"css":"#frame"}]}),
        json!({"shadowPath":[{"css":"#shadow-host"}]}),
    ] {
        for (kind, present) in [("text", "#empty-text"), ("value", "#empty-value")] {
            for matcher in [
                json!({"kind":"exact", "value":""}),
                json!({"kind":"regex", "value":"^$"}),
            ] {
                let mut target = scope.clone();
                target["css"] = json!(present);
                let result = live
                    .call(
                        "wait_for",
                        json!({
                            "condition":{"kind":kind, "target":target, "matcher":matcher},
                            "timeoutMs":1000,
                        }),
                    )
                    .await;
                assert_eq!(result["status"], "completed", "{scope} {kind}: {result}");
                assert_eq!(evidence(&result, "wait")["observed"], "", "{result}");
                target["css"] = json!("#missing");
                let result = live
                    .call(
                        "wait_for",
                        json!({
                            "condition":{"kind":kind, "target":target, "matcher":matcher},
                            "timeoutMs":200,
                        }),
                    )
                    .await;
                if result["status"] != "failed"
                    || result["error"]["code"] != "waitConditionTimedOut"
                {
                    failures.push(format!("{scope} missing {kind} {matcher}: {result}"));
                }
            }
        }
    }
    live.close().await;
    assert!(
        failures.is_empty(),
        "missing targets matched empty text: {failures:#?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chrome or Chromium"]
async fn empty_wait_target_chromium() {
    empty_wait_target_contract(Engine::Chromium).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Firefox and paired test profile"]
async fn empty_wait_target_firefox() {
    empty_wait_target_contract(Engine::Firefox).await;
}

async fn shadow_text_wait_contract(engine: Engine) {
    const CONTROLS: &str = "<p class='status' role='status' aria-label='Progress'>First</p>\
        <p class='status' role='status' aria-label='Progress'>Second</p>\
        <p class='status' id='status' data-testid='status' role='status' aria-label='Progress'>Inside</p>\
        <input class='code' role='textbox' aria-label='Code' value='First'>\
        <input class='code' role='textbox' aria-label='Code' value='Second'>\
        <input class='code' id='code' data-testid='code' role='textbox' aria-label='Code' value='Inside'>";
    let document = format!(
        "<!doctype html><title>Shadow text wait contract</title>\
         <p class='status' id='status' data-testid='status' role='status' aria-label='Progress'>Outside</p>\
         <input class='code' id='code' data-testid='code' role='textbox' aria-label='Code' value='Outside'>\
         <div id='shadow-host' role='group' aria-label='Widget'></div>\
         <script>document.querySelector('#shadow-host').attachShadow({{mode:'open'}}).innerHTML={};</script>",
        json!(CONTROLS),
    );
    let site = FixtureSite::spawn(vec![
        (
            "/",
            Route::Html(format!(
                "{document}<iframe id='frame' src='/frame'></iframe>"
            )),
        ),
        ("/frame", Route::Html(document)),
    ])
    .await;
    let rig = match engine {
        Engine::Chromium => Rig::chromium().await,
        Engine::Firefox => Rig::firefox().await,
    };
    let live = Live::open(&rig, &site.url("/")).await;
    let mut failures = Vec::new();
    for frame_path in [json!([]), json!([{"css":"#frame"}])] {
        for (kind, css, tag, role, name) in [
            ("text", "status", "p", "status", "Progress"),
            ("value", "code", "input", "textbox", "Code"),
        ] {
            for mut target in [
                json!({"css":format!("#{css}")}),
                json!({"css":tag}),
                json!({"testId":css}),
                json!({"role":role, "accessibleName":name}),
            ] {
                target["framePath"] = frame_path.clone();
                target["shadowPath"] = json!([{"role":"group", "accessibleName":"Widget"}]);
                for (expected, status) in [("Inside", "completed"), ("Outside", "failed")] {
                    let timeout_ms = if status == "completed" { 5000 } else { 300 };
                    let result = live.call("wait_for", json!({
                        "condition":{"kind":kind, "target":target, "matcher":{"kind":"exact", "value":expected}},
                        "timeoutMs":timeout_ms,
                    })).await;
                    if result["status"] != status
                        || (status == "failed"
                            && result["error"]["code"] != "waitConditionTimedOut")
                    {
                        failures.push(format!("{kind} {target} expected {expected}: {result}"));
                    } else if status == "completed" {
                        assert_eq!(evidence(&result, "wait")["observed"], "Inside", "{result}");
                    }
                }
            }
        }
    }
    live.close().await;
    assert!(
        failures.is_empty(),
        "shadow waits escaped their scope: {failures:#?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chrome or Chromium"]
async fn shadow_text_wait_chromium() {
    shadow_text_wait_contract(Engine::Chromium).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Firefox and paired test profile"]
async fn shadow_text_wait_firefox() {
    shadow_text_wait_contract(Engine::Firefox).await;
}

fn evidence<'a>(outcome: &'a Value, kind: &str) -> &'a Value {
    outcome["evidence"]
        .as_array()
        .expect("command evidence")
        .iter()
        .find(|item| item["kind"] == kind)
        .unwrap_or_else(|| panic!("missing {kind} evidence: {outcome}"))
}

fn assert_materialized_uploads(rig: &Rig, session_id: &Value, expected: usize) {
    let directory = rig
        .storage_root()
        .join("downloads")
        .join(session_id.as_str().unwrap())
        .join("upload-artifacts");
    let count = if directory.exists() {
        std::fs::read_dir(directory).unwrap().count()
    } else {
        0
    };
    assert_eq!(count, expected, "unexpected retained artifact uploads");
}

async fn artifact_upload_contract(engine: Engine) {
    const PAYLOAD: &str = "artifact upload contract: café\nsecond line\n";
    let site = FixtureSite::spawn(vec![
        ("/", Route::Html(
            "<!doctype html><title>Artifact upload contract</title>\
             <label>Resume<input id='file' type='file'></label>\
             <button onclick=\"document.querySelector('#status').textContent = 'Sending'; document.querySelector('#file').files[0].text().then(body => fetch('/received', {method:'POST', body})).then(() => document.querySelector('#status').textContent = 'Uploaded').catch(error => document.querySelector('#status').textContent = 'Read failed: ' + error.message)\">Send</button>\
             <p id='status'>Pending</p>".into(),
        )),
        ("/received", Route::Raw { content_type: "text/plain", body: "ok".into() }),
    ]).await;
    let rig = match engine {
        Engine::Chromium => Rig::chromium().await,
        Engine::Firefox => Rig::firefox().await,
    };
    let live = Live::open(&rig, &site.url("/")).await;
    let record = rig
        .seed_artifact(&live.session_id, &live.page_id, PAYLOAD.as_bytes())
        .await;
    let id = &record.artifact_id;
    let source = format!("artifact://{id}");
    let uploaded = live
        .call(
            "intent_fill",
            json!({"purpose":"Resume", "hints":{"role":"button", "accessibleName":"Resume"},
                "value":{"kind":"setFiles", "paths":[source]}}),
        )
        .await;
    assert_eq!(uploaded["status"], "completed", "{uploaded}");
    assert_eq!(
        evidence(&uploaded, "upload")["paths"],
        json!([types::upload_source_reference(&source)])
    );
    assert!(
        !uploaded
            .to_string()
            .contains(rig.storage_root().to_str().unwrap()),
        "upload evidence disclosed a storage path: {uploaded}"
    );
    assert_materialized_uploads(&rig, &live.session_id, 1);
    let reused = live
        .call(
            "upload_files",
            json!({"selector":"#file", "paths":[source]}),
        )
        .await;
    assert_eq!(reused["status"], "completed", "{reused}");
    assert_materialized_uploads(&rig, &live.session_id, 1);

    let sent = live.call("click", json!({"selector":"button"})).await;
    assert_eq!(sent["status"], "completed", "{sent}");
    let waited = live.call("wait_for", json!({
        "condition":{"kind":"text", "target":{"css":"#status"}, "matcher":{"kind":"regex", "value":"^(Uploaded|Read failed:.*)$"}},
        "timeoutMs":5000
    })).await;
    assert_eq!(waited["status"], "completed", "{waited}");
    let status = live.call("inspect", json!({"selector":"#status"})).await;
    assert!(
        status.to_string().contains("Uploaded"),
        "browser file read failed: {status}"
    );
    assert_eq!(
        site.bodies("/received"),
        vec![PAYLOAD.as_bytes().to_vec()],
        "the browser must transmit the exact artifact bytes after file selection returns"
    );

    // A second session on the same authenticated runtime cannot use the handle.
    let other = Live::open(&rig, &site.url("/")).await;
    let refused = other
        .call(
            "upload_files",
            json!({"selector":"#file", "paths":[source]}),
        )
        .await;
    assert_eq!(refused["status"], "policyDenied", "{refused}");
    assert_eq!(refused["error"]["code"], "policyDenied", "{refused}");
    assert_materialized_uploads(&rig, &other.session_id, 0);
    other.close().await;

    let secondary = rig
        .seed_artifact(&live.session_id, &live.page_id, b"secondary artifact")
        .await;
    let secondary_source = format!("artifact://{}", secondary.artifact_id);

    // Modify the fixture-owned artifact behind its valid manifest. Admission
    // must re-hash bytes instead of trusting the content-addressed filename.
    let artifact = rig
        .storage_root()
        .join("artifacts")
        .join(live.session_id.as_str().unwrap())
        .join(id);
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(artifact.join(format!("{id}.json"))).unwrap())
            .unwrap();
    std::fs::write(
        artifact.join(manifest["filename"].as_str().unwrap()),
        b"tampered",
    )
    .unwrap();
    for paths in [
        vec![source.clone()],
        vec![secondary_source, source.clone()],
        vec!["artifact://../outside".into()],
        vec![format!("artifact://{}", "0".repeat(64))],
    ] {
        let refused = live
            .call("upload_files", json!({"selector":"#file", "paths":paths}))
            .await;
        assert_eq!(refused["status"], "policyDenied", "{refused}");
        assert_eq!(refused["error"]["code"], "policyDenied", "{refused}");
        assert!(
            !refused
                .to_string()
                .contains(rig.storage_root().to_str().unwrap()),
            "upload refusal disclosed a storage path: {refused}"
        );
        assert_materialized_uploads(&rig, &live.session_id, 1);
    }
    assert_eq!(
        site.hits("/received"),
        1,
        "refused uploads must not submit data"
    );
    let sent = live.call("click", json!({"selector":"button"})).await;
    assert_eq!(sent["status"], "completed", "{sent}");
    let waited = live.call("wait_for", json!({
        "condition":{"kind":"text", "target":{"css":"#status"}, "matcher":{"kind":"exact", "value":"Uploaded"}}, "timeoutMs":5000
    })).await;
    assert_eq!(waited["status"], "completed", "{waited}");
    assert_eq!(
        site.bodies("/received"),
        vec![PAYLOAD.as_bytes().to_vec(); 2],
        "refused sources must leave the previously selected file unchanged"
    );
    let session_id = live.session_id.clone();
    live.close().await;
    assert_materialized_uploads(&rig, &session_id, 0);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chrome or Chromium"]
async fn artifact_upload_chromium() {
    artifact_upload_contract(Engine::Chromium).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Firefox and paired test profile"]
async fn artifact_upload_firefox() {
    artifact_upload_contract(Engine::Firefox).await;
}
