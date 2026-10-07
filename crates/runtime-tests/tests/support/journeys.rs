//! End-to-end agent journeys on local fixture pages. Each journey drives the
//! production MCP server the way an agent does and asserts what the agent
//! sees, including a failure path. The same journeys run on Chromium and
//! Firefox.

use std::path::PathBuf;

use serde_json::{json, Value};

use super::rig::{assert_node, find_node, strings_under, Live, Rig};
use test_site::{FixtureSite, Route};

/// Directories the upload journey uses: one inside the configured upload
/// roots, one outside them.
pub struct Dirs {
    pub allowed: tempfile::TempDir,
    pub outside: tempfile::TempDir,
}

impl Dirs {
    pub fn new() -> Self {
        Self {
            allowed: tempfile::tempdir().expect("create allowed upload dir"),
            outside: tempfile::tempdir().expect("create outside upload dir"),
        }
    }

    pub fn upload_roots(&self) -> Vec<PathBuf> {
        vec![self.allowed.path().to_path_buf()]
    }
}

fn page(title: &str, body: &str) -> String {
    format!("<!doctype html><html><head><meta charset=utf-8><title>{title}</title></head><body>{body}</body></html>")
}

fn url_contains(fragment: &str, timeout_ms: u64) -> Value {
    json!({"condition":{"kind":"url","matcher":{"kind":"contains","value":fragment}},"timeoutMs":timeout_ms})
}

fn assert_completed(result: &Value, what: &str) {
    assert_eq!(result["status"], "completed", "{what}: {result}");
}

fn assert_failed(result: &Value, code: &str, what: &str) {
    assert_ne!(result["status"], "completed", "{what} succeeded: {result}");
    assert_eq!(result["error"]["code"], code, "{what}: {result}");
}

fn reported(result: &Value, key: &str) -> Vec<String> {
    let mut found = Vec::new();
    strings_under(result, key, &mut found);
    found.into_iter().map(str::to_owned).collect()
}

/// J1: a protected page redirects to sign-in, sign-in script-redirects back
/// after a delay, and the app renders its content late. The agent is told
/// about the app, not the sign-in page, and sees the late content. A page
/// that stays on sign-in is reported as sign-in and offers no app controls.
pub async fn j1_sign_in_redirect_then_app(rig: &Rig, _dirs: &Dirs) {
    let app = page(
        "Dashboard",
        r#"<div id="root"><p id="tick">loading</p></div><script>
            // A loader keeps mutating the document until the content lands.
            let ticks = 0;
            const loader = setInterval(() => {
              document.getElementById("tick").textContent = "loading " + ++ticks;
              if (ticks < 8) return;
              clearInterval(loader);
              document.getElementById("root").innerHTML =
                "<main><h1>Orders dashboard</h1><button>Export orders</button></main>";
            }, 100);
        </script>"#,
    );
    let signin = page(
        "Sign in",
        r#"<h1>Sign in</h1><script>setTimeout(() => location.replace("/app"), 300);</script>"#,
    );
    let locked = page("Sign in", "<h1>Sign in required</h1>");
    let site = FixtureSite::spawn(vec![
        (
            "/app",
            Route::RedirectOnce {
                to: "/signin?next=/app".into(),
                then: app,
            },
        ),
        ("/signin", Route::Html(signin)),
        ("/locked", Route::Redirect("/signin-static".into())),
        ("/signin-static", Route::Html(locked)),
    ])
    .await;

    let live = Live::open(rig, &site.url("/app")).await;
    let urls = reported(&live.started, "url");
    let titles = reported(&live.started, "title");
    assert!(
        urls.iter().any(|url| url.ends_with("/app")),
        "workflow_start did not report the app URL: {}",
        live.started
    );
    assert!(
        !urls.iter().any(|url| url.contains("/signin")),
        "workflow_start reported the sign-in URL: {}",
        live.started
    );
    assert!(
        titles.iter().any(|title| title == "Dashboard") && !titles.iter().any(|t| t == "Sign in"),
        "workflow_start did not report the dashboard title: {}",
        live.started
    );
    let observed = live.observe(json!({})).await;
    assert_node(&observed, "heading", Some("Orders dashboard"));
    assert_node(&observed, "button", Some("Export orders"));
    live.close().await;

    let stuck = Live::open(rig, &site.url("/locked")).await;
    let urls = reported(&stuck.started, "url");
    assert!(
        urls.iter().any(|url| url.ends_with("/signin-static")),
        "workflow_start hid the sign-in page the redirect ended on: {}",
        stuck.started
    );
    let followed = stuck
        .call(
            "intent_follow",
            json!({
                "purpose":"Export the orders",
                "hints":{"role":"button","accessibleName":"Export orders"},
                "expectedState": url_contains("/export", 2000)
            }),
        )
        .await;
    assert_ne!(
        followed["status"], "completed",
        "intent_follow claimed to act on a control the sign-in page does not have: {followed}"
    );
    stuck.close().await;
}

/// J2: type into a placeholder-named search box, submit with Enter, follow
/// one of two identically titled results by ordinal, with "password" in
/// the page text. An ambiguous follow is refused instead of guessed.
pub async fn j2_search_and_follow_result(rig: &Rig, _dirs: &Dirs) {
    let search = page(
        "Search",
        r#"<main><h1>Catalog</h1><p>Forgot your password? Reset it in account settings.</p>
        <form action="/results" method="get">
          <input name="q" type="search" placeholder="Search products">
          <button type="submit">Search</button>
        </form></main>"#,
    );
    let results = page(
        "Results",
        r#"<main><h1>Results</h1><p>Never share your password with a seller.</p><ul>
        <li><a href="/item/1">Widget Pro</a></li>
        <li><a href="/item/2">Widget Pro</a></li>
        <li><a href="/item/3">Widget Basic</a></li></ul></main>"#,
    );
    let site = FixtureSite::spawn(vec![
        ("/search", Route::Html(search)),
        ("/results", Route::Html(results)),
        (
            "/item/1",
            Route::Html(page("Item 1", "<main><h1>Item one</h1></main>")),
        ),
        (
            "/item/2",
            Route::Html(page("Item 2", "<main><h1>Item two</h1></main>")),
        ),
        (
            "/item/3",
            Route::Html(page("Item 3", "<main><h1>Item three</h1></main>")),
        ),
    ])
    .await;
    let live = Live::open(rig, &site.url("/search")).await;
    let typed = live
        .call(
            "type_text",
            json!({"target":{"role":"searchbox","accessibleName":"Search products"},
                   "value":"widget","clearFirst":true}),
        )
        .await;
    assert_completed(&typed, "type_text into the placeholder-named box");
    let searched = live
        .call(
            "intent_follow",
            json!({"purpose":"Run the search","hints":{"role":"button","accessibleName":"Search"},
                   "expectedDestination": url_contains("/results", 5000)}),
        )
        .await;
    assert_completed(&searched, "intent_follow on the Search button");
    assert_eq!(
        site.hits("/results"),
        1,
        "the search was not submitted once"
    );
    let observed = live.observe(json!({})).await;
    assert_node(&observed, "heading", Some("Results"));

    let ambiguous = live
        .call(
            "intent_follow",
            json!({"purpose":"Open the product","hints":{"role":"link","accessibleName":"Widget Pro"},
                   "expectedDestination": url_contains("/item/", 5000)}),
        )
        .await;
    assert_failed(
        &ambiguous,
        "targetAmbiguous",
        "follow of a doubled result title",
    );
    assert_eq!(
        site.hits("/item/1") + site.hits("/item/2"),
        0,
        "an ambiguous follow navigated anyway"
    );

    let followed = live
        .call(
            "intent_follow",
            json!({"purpose":"Open the second Widget Pro",
                   "hints":{"role":"link","accessibleName":"Widget Pro","ordinal":1},
                   "expectedDestination": url_contains("/item/2", 5000)}),
        )
        .await;
    assert_completed(&followed, "intent_follow with an ordinal");
    assert_eq!(site.hits("/item/2"), 1, "the second result was not opened");
    assert_eq!(site.hits("/item/1"), 0, "the first result was opened");
    let item = live.observe(json!({})).await;
    assert_node(&item, "heading", Some("Item two"));
    live.close().await;
}

fn order_form(error: bool) -> String {
    let (invalid, alert) = if error {
        (
            r#" aria-invalid="true""#,
            r#"<p role="alert">Name is required</p>"#,
        )
    } else {
        ("", "")
    };
    page(
        "Order",
        &format!(
            r#"<main><h1>Order</h1>{alert}
            <form id="order" action="/submit" method="post">
              <label>Name <input name="name"{invalid}></label>
              <button type="button" id="more">Add company</button>
              <label id="company-row" style="display:none">Company <input name="company"></label>
              <button type="submit" id="go">Place order</button>
            </form></main>
            <script>
              document.getElementById("more").onclick = () =>
                document.getElementById("company-row").style.display = "block";
              document.getElementById("order").onsubmit = () =>
                setTimeout(() => document.getElementById("go").disabled = true, 0);
            </script>"#
        ),
    )
}

/// J3: a form with a conditional field behind a button, a submit button
/// that disables itself, and a server-side rejection on the first submit.
/// The agent is told the submit was rejected, corrects the field and the
/// second submit succeeds.
pub async fn j3_form_validation_then_success(rig: &Rig, _dirs: &Dirs) {
    let site = FixtureSite::spawn(vec![
        ("/form", Route::Html(order_form(false))),
        ("/form-error", Route::Html(order_form(true))),
        (
            "/submit",
            Route::RedirectOnce {
                to: "/form-error".into(),
                then: page("Done", "<main><h1>Order placed</h1></main>"),
            },
        ),
    ])
    .await;
    let live = Live::open(rig, &site.url("/form")).await;
    let reveal = json!({"role":"button","accessibleName":"Add company"});
    let filled = live
        .call(
            "intent_complete_form",
            json!({"purpose":"Fill the order",
            "fields":[
              {"name":"Name","purpose":"name",
               "value":{"kind":"setText","value":"Ada"}},
              {"name":"Company","purpose":"company",
               "value":{"kind":"setText","value":"Acme"},"revealedBy":reveal},
            ]}),
        )
        .await;
    assert_completed(&filled, "intent_complete_form with a revealed field");
    let submit = json!({
        "purpose":"Place the order",
        "hints":{"role":"button","accessibleName":"Place order"},
        "expectedState":{"condition":{"kind":"networkQuiet","idleMs":300,"maxInFlight":0},"timeoutMs":8000}
    });
    let rejected = live.call("intent_submit_and_verify", submit.clone()).await;
    assert!(
        rejected.to_string().contains("validationRejected"),
        "the first submit was not reported as rejected: {rejected}"
    );
    assert_eq!(site.hits("/submit"), 1);

    let fixed = live
        .call(
            "intent_complete_form",
            json!({"purpose":"Correct the name",
                   "fields":[{"name":"Name","purpose":"name",
                              "value":{"kind":"setText","value":"Ada Lovelace"}}]}),
        )
        .await;
    assert_completed(&fixed, "intent_complete_form after the rejection");
    let mut resubmit = submit;
    resubmit["reSubmit"] = json!(true);
    let accepted = live.call("intent_submit_and_verify", resubmit).await;
    assert_completed(&accepted, "the corrected submit");
    assert!(
        !accepted.to_string().contains("validationRejected"),
        "the corrected submit was still reported as rejected: {accepted}"
    );
    assert_eq!(site.hits("/submit"), 2);
    let done = live.observe(json!({})).await;
    assert_node(&done, "heading", Some("Order placed"));
    live.close().await;
}

/// J4: files go into a hidden input behind a styled button and into a
/// visible one; the page reads the bytes and posts them. A path outside
/// the configured upload roots is refused as `policyDenied` with a repair.
pub async fn j4_upload_hidden_and_visible(rig: &Rig, dirs: &Dirs) {
    let upload = page(
        "Upload",
        r#"<main><h1>Documents</h1>
        <label for="hidden-file" class="btn">Attach resume</label>
        <input type="file" id="hidden-file" style="display:none">
        <input type="file" id="visible-file" aria-label="Visible attachment">
        <pre id="out"></pre></main>
        <script>
          for (const input of document.querySelectorAll("input[type=file]")) {
            input.onchange = async () => {
              const file = input.files[0];
              const text = await file.text();
              await fetch("/upload", {method: "POST", body: text});
              document.getElementById("out").textContent += input.id + "=" + text + ";";
            };
          }
        </script>"#,
    );
    let site = FixtureSite::spawn(vec![
        (
            "/upload",
            Route::Raw {
                content_type: "text/plain",
                body: "ok".into(),
            },
        ),
        ("/page", Route::Html(upload)),
    ])
    .await;
    let resume = dirs.allowed.path().join("resume.txt");
    let photo = dirs.allowed.path().join("photo.txt");
    let secret = dirs.outside.path().join("secret.txt");
    std::fs::write(&resume, "hidden-bytes-7731").expect("write resume");
    std::fs::write(&photo, "visible-bytes-4429").expect("write photo");
    std::fs::write(&secret, "secret-bytes-0001").expect("write secret");

    let live = Live::open(rig, &site.url("/page")).await;
    let hidden = live
        .call(
            "upload_files",
            json!({"selector":"#hidden-file","paths":[resume.to_str().expect("utf8 path")]}),
        )
        .await;
    assert_completed(&hidden, "upload into the hidden input");
    let visible = live
        .call(
            "upload_files",
            json!({"selector":"#visible-file","paths":[photo.to_str().expect("utf8 path")]}),
        )
        .await;
    assert_completed(&visible, "upload into the visible input");
    let mut posted = 0;
    for _ in 0..50 {
        posted = site.hits("/upload");
        if posted >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(posted, 2, "the server did not receive both uploads");
    let observed = live.observe(json!({})).await.to_string();
    for expected in [
        "hidden-file=hidden-bytes-7731",
        "visible-file=visible-bytes-4429",
    ] {
        assert!(
            observed.contains(expected),
            "the page did not read {expected}: {observed}"
        );
    }

    let denied = live
        .call(
            "upload_files",
            json!({"selector":"#visible-file","paths":[secret.to_str().expect("utf8 path")]}),
        )
        .await;
    assert_failed(
        &denied,
        "policyDenied",
        "upload from outside the upload roots",
    );
    let repair = denied["error"]["repair"]["action"]
        .as_str()
        .unwrap_or_default();
    assert!(
        repair.contains("use an allowed path"),
        "policyDenied does not carry the documented repair: {denied}"
    );
    let message = denied["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("outside configured roots") && message.contains("secret.txt"),
        "policyDenied does not name the path and the roots: {denied}"
    );
    assert_eq!(
        site.hits("/upload"),
        2,
        "the denied upload reached the server"
    );
    live.close().await;
}

/// J5: a window.open popup is listed, acted in and closed, then the agent
/// returns to the opener and targets a control inside a same-origin iframe
/// from the snapshot. Acting on the closed popup fails instead of landing
/// on the opener.
pub async fn j5_popup_and_frame(rig: &Rig, _dirs: &Dirs) {
    let main = page(
        "Opener",
        r#"<main><h1>Opener</h1>
        <button onclick="window.open('/popup', 'help', 'width=420,height=320')">Open help</button>
        <iframe src="/frame" title="Widget frame" width="300" height="120"></iframe></main>"#,
    );
    let popup = page(
        "Help",
        r#"<main><h1>Help</h1>
        <button onclick="this.textContent = 'Acknowledged'">Acknowledge</button></main>"#,
    );
    let frame = page(
        "Frame",
        r#"<main><button onclick="this.textContent = 'Frame done'">Frame action</button></main>"#,
    );
    let site = FixtureSite::spawn(vec![
        ("/main", Route::Html(main)),
        ("/popup", Route::Html(popup)),
        ("/frame", Route::Html(frame)),
    ])
    .await;
    let live = Live::open(rig, &site.url("/main")).await;
    let opened = live
        .call(
            "click_and_wait_for_popup",
            json!({"target":{"role":"button","accessibleName":"Open help"},"timeoutMs":10000}),
        )
        .await;
    assert_completed(&opened, "click_and_wait_for_popup");
    let listed = rig
        .tool("page_list", json!({"sessionId":live.session_id}))
        .await;
    let ids = reported(&listed, "pageId");
    let opener = live.page_id.as_str().expect("page id").to_owned();
    let popup_id = ids
        .iter()
        .find(|id| **id != opener)
        .unwrap_or_else(|| panic!("page_list does not show the popup: {listed}"))
        .clone();

    let acked = rig
        .tool(
            "click",
            json!({"sessionId":live.session_id,"pageId":popup_id,
                   "target":{"role":"button","accessibleName":"Acknowledge"}}),
        )
        .await;
    assert_completed(&acked, "click inside the popup");
    let seen = rig
        .tool(
            "workflow_observe",
            json!({"sessionId":live.session_id,"pageId":popup_id}),
        )
        .await;
    assert_node(&seen, "button", Some("Acknowledged"));

    let closed = rig
        .tool(
            "page_close",
            json!({"sessionId":live.session_id,"pageId":popup_id}),
        )
        .await;
    assert_completed(&closed, "page_close of the popup");
    let after = rig
        .tool("page_list", json!({"sessionId":live.session_id}))
        .await;
    assert!(
        !reported(&after, "pageId").contains(&popup_id),
        "page_list still shows the closed popup: {after}"
    );
    let stale = rig
        .tool(
            "click",
            json!({"sessionId":live.session_id,"pageId":popup_id,
                   "target":{"role":"button","accessibleName":"Acknowledge"}}),
        )
        .await;
    assert_ne!(
        stale["status"], "completed",
        "a click on the closed popup succeeded: {stale}"
    );

    let snapshot = live.snapshot(json!({})).await;
    let button = find_node(&snapshot, "button", Some("Frame action"))
        .unwrap_or_else(|| panic!("the snapshot does not show the frame control: {snapshot}"));
    let target = button["target"].clone();
    assert!(
        target.is_object(),
        "the frame control has no target: {button}"
    );
    let clicked = live.call("click", json!({"target":target})).await;
    assert_completed(&clicked, "click through the snapshot target");
    let again = live.snapshot(json!({})).await;
    assert_node(&again, "button", Some("Frame done"));
    live.close().await;
}

/// J6: hop between two origins by link and by navigate, with a snapshot
/// after every hop, then close the session and start a new one.
pub async fn j6_cross_site_navigation_recovery(rig: &Rig, _dirs: &Dirs) {
    let b = FixtureSite::spawn(vec![(
        "/b",
        Route::Html(page("Site B", "<main><h1>Site B</h1></main>")),
    )])
    .await;
    let a_page = page(
        "Site A",
        &format!(
            r#"<main><h1>Site A</h1><a href="{}">Go to B</a></main>"#,
            b.url("/b")
        ),
    );
    let a = FixtureSite::spawn(vec![
        ("/a", Route::Html(a_page)),
        (
            "/a2",
            Route::Html(page("Site A again", "<main><h1>Site A again</h1></main>")),
        ),
    ])
    .await;

    let live = Live::open(rig, &a.url("/a")).await;
    let snapshot = live.snapshot(json!({})).await;
    assert_completed(&snapshot, "snapshot on A");
    assert_node(&snapshot, "heading", Some("Site A"));

    let followed = live
        .call(
            "intent_follow",
            json!({"purpose":"Open site B","hints":{"role":"link","accessibleName":"Go to B"},
                   "expectedDestination": url_contains("/b", 8000)}),
        )
        .await;
    assert_completed(&followed, "cross-origin link to B");
    let snapshot = live.snapshot(json!({})).await;
    assert_completed(&snapshot, "snapshot after the hop to B");
    assert_node(&snapshot, "heading", Some("Site B"));

    for (url, heading, what) in [
        (a.url("/a2"), "Site A again", "navigate back to A"),
        (b.url("/b"), "Site B", "navigate to B again"),
        (a.url("/a2"), "Site A again", "navigate to A again"),
    ] {
        let navigated = live
            .call(
                "navigate",
                json!({"url":url,"waitUntil":"interactive","timeoutMs":15000}),
            )
            .await;
        assert_completed(&navigated, what);
        let snapshot = live.snapshot(json!({})).await;
        assert_completed(&snapshot, &format!("snapshot after {what}"));
        assert_node(&snapshot, "heading", Some(heading));
    }
    live.close().await;

    let fresh = Live::open(rig, &b.url("/b")).await;
    let snapshot = fresh.snapshot(json!({})).await;
    assert_completed(&snapshot, "snapshot in the new session");
    assert_node(&snapshot, "heading", Some("Site B"));
    fresh.close().await;
}
