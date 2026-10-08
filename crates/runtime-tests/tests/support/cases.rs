//! Regression cases reproduced on minimal local pages. Each one is a failure
//! observed on linkedin.com on 2026-10-06 through the real runtime and fixed
//! in PR #612. The same cases run against Chromium and Firefox.

use serde_json::{json, Value};

use super::rig::{assert_node, find_node, strings_under, targets_under, Live, Rig};
use test_site::{FixtureSite, Route};

const HIDDEN_NODES: usize = 3000;

fn page(title: &str, body: &str) -> String {
    format!("<!doctype html><html><head><meta charset=utf-8><title>{title}</title></head><body>{body}</body></html>")
}

fn hidden_menu() -> String {
    let mut menu = String::from(r#"<div style="display:none">"#);
    for index in 0..HIDDEN_NODES {
        menu.push_str(&format!(r##"<a href="#m{index}">Menu {index}</a>"##));
    }
    menu.push_str("</div>");
    menu
}

fn feed_page() -> String {
    let mut items = String::new();
    for index in 0..30 {
        items.push_str(&format!(
            r#"<li aria-label="Feed post"><p>Post {index}</p></li>"#
        ));
    }
    page(
        "Feed",
        &format!(
            r#"<header><nav aria-label="Primary"><a href="/home">Home</a></nav>{}</header><main><ul>{items}</ul></main>"#,
            hidden_menu()
        ),
    )
}

async fn feed_site() -> FixtureSite {
    FixtureSite::spawn(vec![("/feed", Route::Html(feed_page()))]).await
}

/// F1, linkedin.com/feed: the default `workflow_observe` and
/// `a11y_snapshot` with `maxNodes: 120` returned only the page header and
/// its hidden menu subtrees; the `main` landmark and the "Feed post" list
/// items were never reached because hidden subtrees spent the node budget.
pub async fn hidden_subtrees_do_not_spend_the_node_budget(rig: &Rig) {
    let site = feed_site().await;
    let live = Live::open(rig, &site.url("/feed")).await;
    let observed = live.observe(json!({})).await;
    assert_node(&observed, "main", None);
    assert_node(&observed, "listitem", Some("Feed post"));
    let snapshot = live.snapshot(json!({"maxNodes":120})).await;
    assert_node(&snapshot, "main", None);
    assert_node(&snapshot, "listitem", Some("Feed post"));
    live.close().await;
}

/// F2, linkedin.com/feed: an `a11y_snapshot` scoped to `{role: main}`
/// still contained the page `banner`, and a scope naming no region was not
/// reported as `targetNotFound`.
pub async fn snapshot_target_scopes_the_tree(rig: &Rig) {
    let site = feed_site().await;
    let live = Live::open(rig, &site.url("/feed")).await;
    let scoped = live.snapshot(json!({"target":{"role":"main"}})).await;
    assert_eq!(scoped["status"], "completed", "scoped snapshot: {scoped}");
    assert_node(&scoped, "main", None);
    assert!(
        find_node(&scoped, "banner", None).is_none(),
        "a banner node leaked into the main-scoped snapshot: {scoped}"
    );
    let missing = live
        .snapshot(json!({"target":{"role":"main","accessibleName":"no-such-region"}}))
        .await;
    assert_eq!(
        missing["error"]["code"], "targetNotFound",
        "unknown target did not return targetNotFound: {missing}"
    );
    live.close().await;
}

/// F9, linkedin.com/feed: `workflow_start` on the feed URL reported the
/// login URL and login title, because the feed redirected to a login page
/// that replaced itself back to the feed a moment later.
pub async fn workflow_start_reports_the_settled_page(rig: &Rig) {
    let login = page(
        "Login",
        r#"<h1>Login</h1><script>setTimeout(() => location.replace("/feed"), 200);</script>"#,
    );
    let site = FixtureSite::spawn(vec![
        (
            "/feed",
            Route::RedirectOnce {
                to: "/login?next=/feed".into(),
                then: page("Feed", "<main><h1>Feed</h1></main>"),
            },
        ),
        ("/login", Route::Html(login)),
    ])
    .await;
    let live = Live::open(rig, &site.url("/feed")).await;
    let mut urls = Vec::new();
    let mut titles = Vec::new();
    strings_under(&live.started, "url", &mut urls);
    strings_under(&live.started, "title", &mut titles);
    assert!(
        urls.iter().any(|url| url.ends_with("/feed")),
        "workflow_start did not report the feed URL: {}",
        live.started
    );
    assert!(
        !urls.iter().any(|url| url.contains("/login")),
        "workflow_start reported the login URL: {}",
        live.started
    );
    assert!(
        titles.contains(&"Feed") && !titles.contains(&"Login"),
        "workflow_start did not report the feed title: {}",
        live.started
    );
    live.close().await;
}

/// F10, linkedin.com/messaging: a `workflow_observe` issued right after
/// `navigate` returned a near-empty tree because the conversation list is
/// inserted by script some time after load.
pub async fn observe_after_navigate_includes_late_content(rig: &Rig) {
    let late = page(
        "Late",
        r#"<div id="root"><p id="tick">loading</p></div><script>
            // A loader keeps mutating the document until the content lands.
            let ticks = 0;
            const loader = setInterval(() => {
              document.getElementById("tick").textContent = "loading " + ++ticks;
              if (ticks < 8) return;
              clearInterval(loader);
              document.getElementById("root").innerHTML =
                "<main><h1>Late content</h1><button>Late action</button></main>";
            }, 100);
        </script>"#,
    );
    let site = FixtureSite::spawn(vec![
        ("/blank", Route::Html(page("Blank", "<p>Blank</p>"))),
        ("/late", Route::Html(late)),
    ])
    .await;
    let live = Live::open(rig, &site.url("/blank")).await;
    let navigated = live
        .call("navigate", json!({"url":site.url("/late")}))
        .await;
    assert_eq!(navigated["status"], "completed", "navigate: {navigated}");
    let observed = live.observe(json!({})).await;
    assert_node(&observed, "button", Some("Late action"));
    assert_node(&observed, "heading", Some("Late content"));
    live.close().await;
}

/// F11, linkedin.com/feed: controls named only through an svg `<title>`,
/// `aria-labelledby` with two ids, a placeholder, nested spans, or an image
/// `alt` showed up in `a11y_snapshot` with no name, and `type_text` on the
/// search box by `{role: textbox, accessibleName: "Search"}` failed.
pub async fn accessible_names_are_computed(rig: &Rig) {
    let body = r##"
        <button><svg width="16" height="16"><title>Close dialog</title><path d="M0 0h16v16z"/></svg></button>
        <span id="first">First</span> <span id="second">Second</span>
        <button aria-labelledby="first second"></button>
        <input placeholder="Search">
        <h1><span>Nested</span><span> heading</span></h1>
        <a href="#home"><img alt="Home page" width="16" height="16"src="data:image/gif;base64,R0lGODlhAQABAAAAACH5BAEKAAEALAAAAAABAAEAAAICTAEAOw=="></a>"##;
    let site = FixtureSite::spawn(vec![("/names", Route::Html(page("Names", body)))]).await;
    let live = Live::open(rig, &site.url("/names")).await;
    let snapshot = live.snapshot(json!({})).await;
    assert_node(&snapshot, "button", Some("Close dialog"));
    assert_node(&snapshot, "button", Some("First Second"));
    assert_node(&snapshot, "textbox", Some("Search"));
    assert_node(&snapshot, "heading", Some("Nested heading"));
    assert_node(&snapshot, "link", Some("Home page"));
    // Every target the snapshot returns resolves for the action that fits
    // its role. The page's controls are inert, so acting is harmless.
    let mut targets = Vec::new();
    targets_under(&snapshot, &mut targets);
    assert!(targets.len() >= 4, "snapshot targets: {snapshot}");
    for (role, target) in targets {
        let (tool, extra) = if role == "textbox" {
            (
                "type_text",
                json!({"target":target,"value":"x","clearFirst":true}),
            )
        } else {
            ("click", json!({"target":target}))
        };
        let result = live.call(tool, extra).await;
        assert_eq!(
            result["status"], "completed",
            "{tool} on the snapshot target {target}: {result}"
        );
    }
    let typed = live
        .call(
            "type_text",
            json!({"target":{"role":"textbox","accessibleName":"Search"},
                   "value":"bobby","clearFirst":true}),
        )
        .await;
    assert_eq!(typed["status"], "completed", "type_text: {typed}");
    live.close().await;
}

/// F12, linkedin.com/feed: `type_text` into the search box failed because
/// the page text contained the words "token" and "credential" and a
/// "Forgot password?" link; the words alone were treated as secret material.
pub async fn secret_words_are_not_secrets(rig: &Rig) {
    let body = r#"
        <p>Manage your token and credential settings.</p>
        <a href="/reset">Forgot password?</a>
        <input placeholder="Search">"#;
    let site = FixtureSite::spawn(vec![("/words", Route::Html(page("Words", body)))]).await;
    let live = Live::open(rig, &site.url("/words")).await;
    let snapshot = live.snapshot(json!({})).await;
    assert_node(&snapshot, "link", Some("Forgot password?"));
    let typed = live
        .call(
            "type_text",
            json!({"target":{"role":"textbox","accessibleName":"Search"},
                   "value":"bobby","clearFirst":true}),
        )
        .await;
    assert_eq!(typed["status"], "completed", "type_text: {typed}");
    live.close().await;
}

/// Page text shaped like `word: more words` is free text, not a URL with the
/// scheme `word:`: the snapshot reports it and the companion stays connected.
pub async fn scheme_like_text_is_page_text(rig: &Rig) {
    let body = "<main><h1>Notes</h1><p>note: read this</p><p>Status:online</p></main>";
    let site = FixtureSite::spawn(vec![("/note", Route::Html(page("Note", body)))]).await;
    let live = Live::open(rig, &site.url("/note")).await;
    for attempt in ["first", "second"] {
        let snapshot = live.snapshot(json!({})).await;
        assert_eq!(
            snapshot["status"], "completed",
            "{attempt} snapshot: {snapshot}"
        );
        let text = snapshot.to_string();
        assert!(
            text.contains("note: read this") && text.contains("Status:online"),
            "{attempt} snapshot lacks the page text: {snapshot}"
        );
    }
    live.close().await;
}

const BEARER_TOKEN: &str = "Zx9Kq2Lm7Rt4Vw8Yb3Nc6Hd1Jf5Gs0Ae2PuXo7Ti";
const PEM_BODY: &str = "MIIEowIBAAKCAQEAx7Qk2LmZr9VtW4YbNc6HdJf5GsAePuXo7TiKq";

/// F12 counterpart: text that really discloses a credential (an
/// `Authorization: Bearer` header value, a PEM private key block) is never
/// agent-visible, as pinned by `crates/firefox-companion/src/secret_material.rs`.
pub async fn disclosed_credentials_are_withheld(rig: &Rig) {
    assert_eq!(BEARER_TOKEN.len(), 40);
    let body = format!(
        r#"<h2>Authorization: Bearer {BEARER_TOKEN}</h2>
        <pre>-----BEGIN RSA PRIVATE KEY-----
{PEM_BODY}
-----END RSA PRIVATE KEY-----</pre>
        <input placeholder="Search">"#
    );
    let site = FixtureSite::spawn(vec![("/leak", Route::Html(page("Leak", &body)))]).await;
    let live = Live::open(rig, &site.url("/leak")).await;
    let started = live.started.to_string();
    let snapshot = live.snapshot(json!({})).await;
    let observed = live.observe(json!({})).await;
    for (what, result) in [
        ("workflow_start", started),
        ("a11y_snapshot", snapshot.to_string()),
        ("workflow_observe", observed.to_string()),
    ] {
        assert!(
            !result.contains(BEARER_TOKEN),
            "{what} exposed the bearer token: {result}"
        );
        assert!(
            !result.contains(PEM_BODY),
            "{what} exposed the private key body: {result}"
        );
    }
    live.close().await;
}

fn large_page(visible: usize, after: &str) -> String {
    let mut body = String::from(r#"<div style="display:none">"#);
    for index in 0..HIDDEN_NODES {
        body.push_str(&format!(r##"<a href="#h{index}">Hidden {index}</a>"##));
    }
    body.push_str("</div><main>");
    for index in 0..visible {
        body.push_str(&format!("<button>Item {index}</button>"));
    }
    body.push_str(after);
    body.push_str("</main>");
    page("Large", &body)
}

/// F13, linkedin.com/jobs: `intent_follow` with
/// `{role: link, accessibleName: "Show all"}` failed to resolve the link,
/// which sits after thousands of hidden and more than 4096 visible nodes.
pub async fn large_dom_link_resolves_for_intent_follow(rig: &Rig) {
    let site = FixtureSite::spawn(vec![
        (
            "/jobs",
            Route::Html(large_page(4500, r#"<a href="/all">Show all</a>"#)),
        ),
        (
            "/all",
            Route::Html(page("All", "<main><h1>All jobs</h1></main>")),
        ),
    ])
    .await;
    let live = Live::open(rig, &site.url("/jobs")).await;
    let followed = live
        .call(
            "intent_follow",
            json!({
                "purpose":"Open the full collection",
                "hints":{"role":"link","accessibleName":"Show all"},
                "expectedDestination":{
                    "condition":{"kind":"url","matcher":{"kind":"contains","value":"/all"}},
                    "timeoutMs":15000
                }
            }),
        )
        .await;
    assert_eq!(followed["status"], "completed", "intent_follow: {followed}");
    live.close().await;
}

/// F13 counterpart: when the target is absent from a page larger than the
/// Firefox companion's candidate cap, the failure says the candidate set was
/// truncated (`resourceExhausted`) instead of reporting `targetNotFound`.
/// Firefox only; Chromium has no candidate cap.
pub async fn oversized_page_reports_truncation_not_target_not_found(rig: &Rig) {
    let site = FixtureSite::spawn(vec![("/jobs", Route::Html(large_page(4500, "")))]).await;
    let live = Live::open(rig, &site.url("/jobs")).await;
    let followed = live
        .call(
            "intent_follow",
            json!({
                "purpose":"Open the full collection",
                "hints":{"role":"link","accessibleName":"Show all"},
                "expectedDestination":{
                    "condition":{"kind":"url","matcher":{"kind":"contains","value":"/all"}},
                    "timeoutMs":5000
                }
            }),
        )
        .await;
    assert_eq!(
        followed["error"]["code"], "resourceExhausted",
        "intent_follow on an oversized page: {followed}"
    );
    let message = followed["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("truncated"),
        "the error does not name the truncation: {followed}"
    );
    live.close().await;
}

const TRACKING_ID: &str = "Zx9Kq2Lm7Rt4Vw8Yb3Nc6Hd1Jf5Gs0Ae2PuXo7Ti9QaB4cDe";
const CSRF_TOKEN: &str = "Qm41ZzK2xP9vLr7TnW3bYc8Hd5Jf6GsA";

/// linkedin.com/feed, Firefox: `type_text` into the search box and
/// `intent_follow` on "Show all" failed with "extension observation
/// contained unsanitized sensitive material" because one page field
/// matched the secret rule and the whole observation was rejected. A
/// matching field is redacted instead and the action proceeds without
/// exposing the secret.
pub async fn redacted_page_fields_do_not_block_actions(rig: &Rig) {
    assert_eq!(TRACKING_ID.len(), 48);
    let body = format!(
        r#"<main>
        <p>Tracking <span>{TRACKING_ID}</span></p>
        <p><span>Session token:</span><span>k8Qm41ZzK2</span></p>
        <form><input type="hidden" name="csrfToken" value="{CSRF_TOKEN}">
        <input placeholder="Search"></form>
        <a href="/all">Show all</a></main>"#
    );
    let site = FixtureSite::spawn(vec![
        ("/tracked", Route::Html(page("Tracked", &body))),
        (
            "/all",
            Route::Html(page("All", "<main><h1>All results</h1></main>")),
        ),
    ])
    .await;
    let live = Live::open(rig, &site.url("/tracked")).await;
    let typed = live
        .call(
            "type_text",
            json!({"target":{"role":"textbox","accessibleName":"Search"},
                   "value":"bobby","clearFirst":true}),
        )
        .await;
    assert_eq!(typed["status"], "completed", "type_text: {typed}");
    let typed_text = typed.to_string();
    for secret in [TRACKING_ID, CSRF_TOKEN, "k8Qm41ZzK2"] {
        assert!(
            !typed_text.contains(secret),
            "type_text exposed page secret material: {typed}"
        );
    }
    let followed = live
        .call(
            "intent_follow",
            json!({
                "purpose":"Open every result",
                "hints":{"role":"link","accessibleName":"Show all"},
                "expectedDestination":{
                    "condition":{"kind":"url","matcher":{"kind":"contains","value":"/all"}},
                    "timeoutMs":15000
                }
            }),
        )
        .await;
    assert_eq!(followed["status"], "completed", "intent_follow: {followed}");
    live.close().await;
}

/// linkedin.com/feed: `a11y_snapshot` with `maxNodes: 120` returned only the
/// header and `truncated: true` because a node was counted after its
/// descendants, so a subtree that ran out of budget discarded its own root
/// and every node already built under it. A node reserves its slot first.
pub async fn snapshot_budget_keeps_ancestors_of_kept_nodes(rig: &Rig) {
    let mut buttons = String::new();
    for index in 0..200 {
        buttons.push_str(&format!("<button>Item {index}</button>"));
    }
    let body =
        format!(r#"<header><a href="/home">Home</a></header><main><div>{buttons}</div></main>"#);
    let site = FixtureSite::spawn(vec![("/deep", Route::Html(page("Deep", &body)))]).await;
    let live = Live::open(rig, &site.url("/deep")).await;
    let snapshot = live.snapshot(json!({"maxNodes":60})).await;
    assert_node(&snapshot, "main", None);
    assert_node(&snapshot, "button", Some("Item 0"));
    assert!(
        find_node(&snapshot, "button", Some("Item 199")).is_none(),
        "the snapshot was not bounded by maxNodes: {snapshot}"
    );
    assert!(
        snapshot.to_string().contains(r#""truncated":true"#),
        "the snapshot does not report truncation: {snapshot}"
    );
    live.close().await;
}

fn nodes_with_role<'a>(value: &'a Value, role: &str, out: &mut Vec<&'a Value>) {
    match value {
        Value::Object(map) => {
            if map.get("role").and_then(Value::as_str) == Some(role) {
                out.push(value);
            }
            map.values()
                .for_each(|child| nodes_with_role(child, role, out));
        }
        Value::Array(items) => items
            .iter()
            .for_each(|child| nodes_with_role(child, role, out)),
        _ => {}
    }
}

/// linkedin.com/feed: `main`, `banner`, `navigation`, `list`, `form` and
/// `contentinfo` were named by the concatenated text of every descendant,
/// bloating each snapshot. Only roles that take their name from content are
/// named from it; `listitem` keeps its text on purpose.
pub async fn containers_are_not_named_from_content(rig: &Rig) {
    let body = r#"<header>Site banner text</header>
        <nav>Navigation words <a href="/a">Alpha</a></nav>
        <main><p>Main body words</p>
          <form>Form words <input placeholder="Search"></form>
          <ul><li>First post text</li><li>Second post text</li></ul>
        </main>
        <footer>Footer words</footer>"#;
    let site = FixtureSite::spawn(vec![("/names", Route::Html(page("Containers", body)))]).await;
    let live = Live::open(rig, &site.url("/names")).await;
    let snapshot = live.snapshot(json!({})).await;
    for role in [
        "banner",
        "main",
        "navigation",
        "form",
        "list",
        "contentinfo",
    ] {
        let mut nodes = Vec::new();
        nodes_with_role(&snapshot, role, &mut nodes);
        assert!(!nodes.is_empty(), "no {role} node: {snapshot}");
        for node in nodes {
            assert!(
                node.get("name").is_none_or(|name| name.is_null()),
                "{role} is named from its content: {node}"
            );
        }
    }
    // Chromium reports the item text as its StaticText child; the Firefox
    // companion names the listitem itself.
    let item_role = if rig.is_firefox() {
        "listitem"
    } else {
        "StaticText"
    };
    assert_node(&snapshot, item_role, Some("First post text"));
    assert_node(&snapshot, item_role, Some("Second post text"));
    assert_node(&snapshot, "link", Some("Alpha"));
    live.close().await;
}

/// The value of the first `textbox` node named `name`.
fn textbox_value<'a>(snapshot: &'a Value, name: &str) -> Option<&'a str> {
    find_node(snapshot, "textbox", Some(name)).and_then(|node| node["value"].as_str())
}

/// L4, linkedin.com/feed, Firefox: `type_text` with the snapshot's
/// `{role: textbox, accessibleName: "I'm looking for…"}` failed with "Origin
/// element ... is not displayed": a hidden duplicate earlier in the page
/// shares the control's identifying attribute. A control below the fold
/// takes text as well.
pub async fn type_text_reaches_the_visible_duplicate(rig: &Rig) {
    let body = r#"<header>
        <div style="display:none"><input name="keywords" placeholder="I'm looking for…"></div>
        <input name="keywords" placeholder="I'm looking for…"></header>
        <main><div style="height:3000px"></div><input name="later" placeholder="Below the fold"></main>"#;
    let site = FixtureSite::spawn(vec![("/feed", Route::Html(page("Feed", body)))]).await;
    let live = Live::open(rig, &site.url("/feed")).await;
    for (name, value) in [
        ("I'm looking for…", "rust engineer"),
        ("Below the fold", "later text"),
    ] {
        let typed = live
            .call(
                "type_text",
                json!({"target":{"role":"textbox","accessibleName":name},
                       "value":value,"clearFirst":true}),
            )
            .await;
        assert_eq!(typed["status"], "completed", "type_text {name}: {typed}");
        let snapshot = live.snapshot(json!({})).await;
        assert_eq!(
            textbox_value(&snapshot, name),
            Some(value),
            "the visible {name} textbox did not take the text: {snapshot}"
        );
    }
    live.close().await;
}

/// L5, linkedin.com/jobs, Firefox: `intent_follow` on `{role: link,
/// accessibleName: "Show all"}` clicked, but the page never navigated: the
/// link shares its identifying attribute with a hidden menu copy and an
/// element that is not a link.
pub async fn intent_follow_clicks_the_visible_duplicate(rig: &Rig) {
    let body = r#"<nav><div role="menu" style="display:none">
          <a href="/jobs/wrong-menu" data-test="show-all">Show all</a></div>
          <span data-test="show-all">Top picks</span></nav>
        <main><section><div style="display:none">
          <a href="/jobs/wrong-hidden" data-test="show-all">Show all</a></div>
          <a href="/jobs/collections/recommended" data-test="show-all">Show all</a></section></main>"#;
    let site = FixtureSite::spawn(vec![
        ("/jobs", Route::Html(page("Jobs", body))),
        (
            "/jobs/collections/recommended",
            Route::Html(page("Collection", "<main><h1>Collection</h1></main>")),
        ),
    ])
    .await;
    let live = Live::open(rig, &site.url("/jobs")).await;
    let followed = live
        .call(
            "intent_follow",
            json!({
                "purpose":"Open the full collection",
                "hints":{"role":"link","accessibleName":"Show all"},
                "expectedDestination":{
                    "condition":{"kind":"url","matcher":{"kind":"contains","value":"/jobs/collections"}},
                    "timeoutMs":15000
                }
            }),
        )
        .await;
    assert_eq!(followed["status"], "completed", "intent_follow: {followed}");
    live.close().await;
}

/// L6, linkedin.com/messaging, Firefox: the full snapshot listed `{role:
/// list, accessibleName: "Conversation List"}` (named by `aria-label`), and
/// a snapshot scoped to exactly that target failed with targetNotFound.
pub async fn snapshot_scopes_to_a_named_list(rig: &Rig) {
    let mut items = String::new();
    for index in 0..40 {
        items.push_str(&format!(
            r##"<li><a href="#c{index}">Conversation {index}</a></li>"##
        ));
    }
    let body = format!(
        r#"<header><nav aria-label="Primary"><a href="/feed">Home</a></nav></header>
        <div style="display:none"><ul aria-label="Conversation List"><li>Stale</li></ul></div>
        <main><div><h2>Messaging</h2><ul aria-label="Conversation List">{items}</ul></div></main>"#
    );
    let site =
        FixtureSite::spawn(vec![("/messaging", Route::Html(page("Messaging", &body)))]).await;
    let live = Live::open(rig, &site.url("/messaging")).await;
    let full = live.snapshot(json!({})).await;
    assert_node(&full, "list", Some("Conversation List"));
    let scoped = live
        .snapshot(json!({"target":{"role":"list","accessibleName":"Conversation List"}}))
        .await;
    assert_eq!(scoped["status"], "completed", "scoped snapshot: {scoped}");
    assert_node(&scoped, "link", Some("Conversation 39"));
    assert!(
        find_node(&scoped, "navigation", None).is_none(),
        "the scoped snapshot leaked the page navigation: {scoped}"
    );
    let missing = live
        .snapshot(json!({"target":{"role":"list","accessibleName":"Missing List"}}))
        .await;
    assert_eq!(
        missing["error"]["code"], "targetNotFound",
        "a missing scope is not targetNotFound: {missing}"
    );
    if rig.is_firefox() {
        let message = missing["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("(targetNotFound): the target was not found on the page"),
            "the failure does not carry the content script's reason: {missing}"
        );
        assert!(
            !message.contains("Missing List"),
            "the failure echoed the caller's value: {missing}"
        );
    }
    live.close().await;
}

/// Every action target an `a11y_snapshot` returns resolves to the element
/// the snapshot described, on a page whose hidden duplicates share an id, a
/// name, or a test id with the visible controls, and whose visible
/// duplicates are told apart by ordinal. Each click names the element it
/// reached in the status region.
pub async fn snapshot_targets_act_on_the_described_element(rig: &Rig) {
    let body = r##"<div style="display:none">
          <button id="save" data-id="hidden-save">Save</button>
          <button name="apply" data-id="hidden-apply">Apply</button>
          <a href="#" data-testid="more" data-id="hidden-more">More</a></div>
        <main>
          <button id="save" data-id="save">Save</button>
          <button name="apply" data-id="apply-0">Apply</button>
          <button data-id="apply-1">Apply</button>
          <a href="#" data-testid="more" data-id="more">More</a>
          <p role="status" aria-label="idle" id="status"></p></main>
        <script>
          document.addEventListener("click", (event) => {
            const reached = event.target.closest("[data-id]");
            if (reached) document.getElementById("status").setAttribute("aria-label", "acted " + reached.dataset.id);
            event.preventDefault();
          }, true);
        </script>"##;
    let site = FixtureSite::spawn(vec![("/dups", Route::Html(page("Duplicates", body)))]).await;
    let live = Live::open(rig, &site.url("/dups")).await;
    let snapshot = live.snapshot(json!({})).await;
    let mut targets = Vec::new();
    targets_under(&snapshot, &mut targets);
    let mut expected = Vec::new();
    for (_, target) in &targets {
        let name = target["accessibleName"].as_str().unwrap_or_default();
        let ordinal = target["ordinal"].as_u64();
        let reached = match (name, ordinal) {
            ("Save", None) => "save",
            ("Apply", Some(0)) => "apply-0",
            ("Apply", Some(1)) => "apply-1",
            ("More", None) => "more",
            _ => panic!("unexpected snapshot target {target}: {snapshot}"),
        };
        expected.push(((*target).clone(), reached));
    }
    assert_eq!(expected.len(), 4, "snapshot targets: {snapshot}");
    for (target, reached) in expected {
        let clicked = live.call("click", json!({"target":target})).await;
        assert_eq!(clicked["status"], "completed", "click {target}: {clicked}");
        let after = live.snapshot(json!({})).await;
        let status = format!("acted {reached}");
        assert!(
            find_node(&after, "status", Some(&status)).is_some(),
            "click {target} did not reach {reached}: {after}"
        );
    }
    live.close().await;
}

/// Every object under `value` whose `kind` is `kind`.
fn objects_of_kind<'a>(value: &'a Value, kind: &str, out: &mut Vec<&'a Value>) {
    match value {
        Value::Object(map) => {
            if map.get("kind").and_then(Value::as_str) == Some(kind) {
                out.push(value);
            }
            for child in map.values() {
                objects_of_kind(child, kind, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                objects_of_kind(item, kind, out);
            }
        }
        _ => {}
    }
}

/// L7, linkedin.com/feed: `type_text` with "rust engineer\n" into the
/// header search box reported a navigation to the results path without its
/// query and with the previous page's title. Enter pushes a URL with a query
/// and the page keeps changing until the results and the new title land.
pub async fn type_text_enter_reports_the_settled_page(rig: &Rig) {
    let home = page(
        "Home",
        r#"<header><input id="q" aria-label="Search"></header><main id="main"><h1>Home feed</h1></main>
        <script>
            document.getElementById("q").addEventListener("keydown", (event) => {
              if (event.key !== "Enter") return;
              event.preventDefault();
              const value = event.target.value;
              history.pushState({}, "", "/search/results/?keywords=" + encodeURIComponent(value) + "&origin=HEADER");
              const main = document.getElementById("main");
              main.innerHTML = "<p id='tick'>loading</p>";
              let ticks = 0;
              const loader = setInterval(() => {
                document.getElementById("tick").textContent = "loading " + ++ticks;
                if (ticks < 25) return;
                clearInterval(loader);
                document.title = "Search results";
                main.innerHTML = "<h1>Results</h1>";
              }, 100);
            });
        </script>"#,
    );
    let site = FixtureSite::spawn(vec![("/home", Route::Html(home))]).await;
    let live = Live::open(rig, &site.url("/home")).await;
    let typed = live
        .call(
            "type_text",
            json!({"target":{"role":"textbox","accessibleName":"Search"},
                   "value":"rust engineer\n","clearFirst":true}),
        )
        .await;
    assert_eq!(typed["status"], "completed", "type_text: {typed}");
    let mut navigations = Vec::new();
    objects_of_kind(&typed, "navigation", &mut navigations);
    let expected_url = site.url("/search/results/?keywords=rust%20engineer&origin=HEADER");
    assert!(
        navigations.iter().any(|item| item["url"] == expected_url.as_str()
            && item["title"] == "Search results"),
        "type_text did not report the settled page {expected_url} titled \"Search results\": {typed}"
    );
    // One call reports one page: every page field in the evidence is the
    // page `page_list` shows.
    let (listed_url, listed_title) = listed_page(&live).await;
    for (key, listed) in [("url", &listed_url), ("title", &listed_title)] {
        let mut reported = Vec::new();
        strings_under(&typed["evidence"], key, &mut reported);
        for value in reported.into_iter().filter(|value| !value.is_empty()) {
            assert_eq!(
                value,
                listed.as_str(),
                "type_text evidence {key} is not the page page_list shows: {typed}"
            );
        }
    }
    live.close().await;
}

/// The URL and title `page_list` reports for the live page.
async fn listed_page(live: &Live<'_>) -> (String, String) {
    let listed = live
        .rig
        .tool("page_list", json!({"sessionId":live.session_id}))
        .await;
    let mut pages = Vec::new();
    objects_with_page_id(&listed, &live.page_id, &mut pages);
    let page = pages
        .into_iter()
        .find(|page| page.get("url").is_some())
        .unwrap_or_else(|| panic!("page_list does not list the page: {listed}"));
    (
        page["url"].as_str().unwrap_or_default().to_owned(),
        page["title"].as_str().unwrap_or_default().to_owned(),
    )
}

fn objects_with_page_id<'a>(value: &'a Value, page_id: &Value, out: &mut Vec<&'a Value>) {
    match value {
        Value::Object(map) => {
            if map.get("pageId") == Some(page_id) {
                out.push(value);
            }
            for child in map.values() {
                objects_with_page_id(child, page_id, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                objects_with_page_id(item, page_id, out);
            }
        }
        _ => {}
    }
}

/// L8, linkedin.com/jobs: `intent_follow` whose link pushes the new URL
/// and renders the page later completed as soon as the URL matched, and its
/// `postState` was the loading skeleton.
pub async fn intent_follow_post_state_shows_the_settled_page(rig: &Rig) {
    let jobs = page(
        "Jobs",
        r#"<main id="main"><a id="go" href="/jobs/collections/recommended">Recommended jobs</a></main>
        <script>
            document.getElementById("go").addEventListener("click", (event) => {
              event.preventDefault();
              history.pushState({}, "", "/jobs/collections/recommended");
              const main = document.getElementById("main");
              main.innerHTML = "<div role='presentation'></div>".repeat(19);
              let ticks = 0;
              const loader = setInterval(() => {
                main.firstChild.setAttribute("data-tick", String(++ticks));
                if (ticks < 15) return;
                clearInterval(loader);
                document.title = "Recommended";
                main.innerHTML = "<h1>Recommended jobs</h1><ul><li><a href='/jobs/view/1'>Rust Engineer</a></li></ul>";
              }, 100);
            });
        </script>"#,
    );
    let site = FixtureSite::spawn(vec![("/jobs", Route::Html(jobs))]).await;
    let live = Live::open(rig, &site.url("/jobs")).await;
    let followed = live
        .call(
            "intent_follow",
            json!({
                "purpose":"Open the recommended jobs",
                "hints":{"role":"link","accessibleName":"Recommended jobs"},
                "expectedState":{
                    "condition":{"kind":"url","matcher":{"kind":"contains","value":"/jobs/"}},
                    "timeoutMs":15000
                }
            }),
        )
        .await;
    assert_eq!(followed["status"], "completed", "intent_follow: {followed}");
    assert!(
        find_node(&followed["postState"], "heading", Some("Recommended jobs")).is_some(),
        "intent_follow postState is not the rendered page: {followed}"
    );
    live.close().await;
}

/// L9, linkedin.com/search: `intent_follow` on "Show all" right after the
/// results started loading failed with targetNotFound at once; the same
/// target clicked seconds later. Each action waits for its target to appear.
pub async fn actions_wait_for_a_late_target(rig: &Rig) {
    let late = page(
        "Late",
        r#"<main><h1>Late controls</h1><p role="status" id="status">waiting</p><div id="slot"></div></main>
        <script>
            setTimeout(() => {
              document.getElementById("slot").innerHTML =
                "<button id='late-button'>Late button</button>" +
                "<input aria-label='Late field'>" +
                "<label><input type='checkbox'> Late option</label>" +
                "<a href='/jobs/all'>Show all</a>";
              document.getElementById("late-button").addEventListener("click", () => {
                document.getElementById("status").textContent = "clicked";
              });
            }, 3000);
        </script>"#,
    );
    let site = FixtureSite::spawn(vec![
        ("/late", Route::Html(late)),
        (
            "/jobs/all",
            Route::Html(page("All jobs", "<main><h1>All jobs</h1></main>")),
        ),
    ])
    .await;
    let live = Live::open(rig, &site.url("/jobs/all")).await;
    let actions = [
        (
            "click",
            json!({"target":{"role":"button","accessibleName":"Late button"}}),
        ),
        (
            "type_text",
            json!({"target":{"role":"textbox","accessibleName":"Late field"},
                   "value":"late text","clearFirst":true}),
        ),
        (
            "control_action",
            json!({"target":{"role":"checkbox","accessibleName":"Late option"},
                   "action":{"kind":"setChecked","checked":true}}),
        ),
        (
            "intent_follow",
            json!({
                "purpose":"Show every job",
                "hints":{"role":"link","accessibleName":"Show all"},
                "expectedState":{
                    "condition":{"kind":"url","matcher":{"kind":"contains","value":"/jobs/all"}},
                    "timeoutMs":15000
                }
            }),
        ),
        (
            "intent_complete_form",
            json!({"purpose":"Fill the late field",
                   "fields":[{"name":"Late field","purpose":"late field",
                              "value":{"kind":"setText","value":"late text"},
                              "hints":{"role":"textbox"}}]}),
        ),
        (
            "intent_submit_and_verify",
            json!({
                "purpose":"Press the late button",
                "hints":{"role":"button","accessibleName":"Late button"},
                "expectedState":{
                    "condition":{"kind":"text","target":{"css":"#status"},
                                 "matcher":{"kind":"contains","value":"clicked"}},
                    "timeoutMs":5000
                }
            }),
        ),
    ];
    let mut failures = Vec::new();
    for (tool, arguments) in actions {
        let loaded = live
            .call("navigate", json!({"url":site.url("/late")}))
            .await;
        assert_eq!(loaded["status"], "completed", "navigate: {loaded}");
        let acted = live.call(tool, arguments).await;
        if acted["status"] != "completed" {
            failures.push(format!("{tool}: {acted}"));
        }
    }
    assert!(
        failures.is_empty(),
        "actions did not wait for their late target: {failures:#?}"
    );
    live.close().await;
}

/// The one wait for a target that never appears: each action fails with
/// targetNotFound after the 5 s target wait, not sooner and not after
/// stacked per-tool retries.
pub async fn actions_fail_a_missing_target_within_one_bound(rig: &Rig) {
    let site = FixtureSite::spawn(vec![(
        "/empty",
        Route::Html(page("Empty", "<main><h1>Nothing here</h1></main>")),
    )])
    .await;
    let live = Live::open(rig, &site.url("/empty")).await;
    let actions = [
        (
            "click",
            json!({"target":{"role":"button","accessibleName":"Never"}}),
        ),
        (
            "type_text",
            json!({"target":{"role":"textbox","accessibleName":"Never"},"value":"x"}),
        ),
        (
            "control_action",
            json!({"target":{"role":"checkbox","accessibleName":"Never"},
                   "action":{"kind":"setChecked","checked":true}}),
        ),
        (
            "intent_follow",
            json!({
                "purpose":"Open the missing link",
                "hints":{"role":"link","accessibleName":"Never"},
                "expectedState":{
                    "condition":{"kind":"url","matcher":{"kind":"contains","value":"/never"}},
                    "timeoutMs":5000
                }
            }),
        ),
        (
            "intent_complete_form",
            json!({"purpose":"Fill the missing field",
                   "fields":[{"name":"Never","purpose":"missing field",
                              "value":{"kind":"setText","value":"x"},
                              "hints":{"role":"textbox"}}]}),
        ),
        (
            "intent_submit_and_verify",
            json!({
                "purpose":"Press the missing button",
                "hints":{"role":"button","accessibleName":"Never"},
                "expectedState":{
                    "condition":{"kind":"url","matcher":{"kind":"contains","value":"/never"}},
                    "timeoutMs":5000
                }
            }),
        ),
    ];
    let mut failures = Vec::new();
    for (tool, arguments) in actions {
        let started = std::time::Instant::now();
        let acted = live.call(tool, arguments).await;
        let elapsed = started.elapsed();
        if acted["error"]["code"] != "targetNotFound" {
            failures.push(format!("{tool} did not fail targetNotFound: {acted}"));
        } else if elapsed < std::time::Duration::from_millis(4_500)
            || elapsed > std::time::Duration::from_millis(8_000)
        {
            failures.push(format!("{tool} failed after {elapsed:?}, not one 5 s wait"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
    live.close().await;
}

/// F5, linkedin.com/feed, Firefox after a restart: `workflow_start`
/// returned before the app's scripts ran, and an immediate `type_text` with
/// Enter submitted the server-rendered search form natively. Here the
/// handler comes from a script loaded after the page's load event, behind a
/// slow script that holds the load event back.
pub async fn navigate_waits_for_late_scripts(rig: &Rig) {
    let search = page(
        "Search",
        r#"<main><form action="/native" method="get"><input aria-label="Search" name="q"></form>
        <p role="status" id="status">waiting</p></main>
        <script async src="/slow-analytics.js"></script>
        <script>
            window.addEventListener("load", () => {
              const app = document.createElement("script");
              app.src = "/app.js";
              document.head.appendChild(app);
            });
        </script>"#,
    );
    let app = r#"document.querySelector("form").addEventListener("submit", (event) => {
          event.preventDefault();
          document.getElementById("status").textContent =
            "handled " + new FormData(event.target).get("q");
        });"#;
    let site = FixtureSite::spawn(vec![
        ("/search", Route::Html(search)),
        (
            "/native",
            Route::Html(page("Native", "<p>native submit</p>")),
        ),
        ("/blank", Route::Html(page("Blank", "<p>Blank</p>"))),
        (
            "/slow-analytics.js",
            Route::Delayed {
                delay: std::time::Duration::from_millis(1_000),
                content_type: "text/javascript",
                body: String::new(),
            },
        ),
        (
            "/app.js",
            Route::Delayed {
                delay: std::time::Duration::from_millis(3_200),
                content_type: "text/javascript",
                body: app.to_owned(),
            },
        ),
    ])
    .await;
    let live = Live::open(rig, &site.url("/blank")).await;
    let navigated = live
        .call("navigate", json!({"url":site.url("/search")}))
        .await;
    assert_eq!(navigated["status"], "completed", "navigate: {navigated}");
    let typed = live
        .call(
            "type_text",
            json!({"target":{"role":"textbox","accessibleName":"Search"},
                   "value":"rust\n","clearFirst":true}),
        )
        .await;
    assert_eq!(typed["status"], "completed", "type_text: {typed}");
    assert_eq!(
        site.hits("/native"),
        0,
        "Enter submitted the form natively before the app's handler was attached: {typed}"
    );
    let snapshot = live.snapshot(json!({})).await;
    let mut names = Vec::new();
    strings_under(&snapshot, "name", &mut names);
    assert!(
        names.contains(&"handled rust"),
        "the app's submit handler did not run: {snapshot}"
    );
    live.close().await;
}

/// A page title that discloses a credential is withheld the same way on
/// `navigate` and `page_list`, on both engines.
pub async fn page_titles_withhold_disclosed_credentials(rig: &Rig) {
    let leaky = format!(
        "<!doctype html><html><head><meta charset=utf-8><title>Authorization: Bearer {BEARER_TOKEN}</title></head><body><p>Leaky title</p></body></html>"
    );
    let site = FixtureSite::spawn(vec![
        ("/blank", Route::Html(page("Blank", "<p>Blank</p>"))),
        ("/leak", Route::Html(leaky)),
    ])
    .await;
    let live = Live::open(rig, &site.url("/blank")).await;
    let navigated = live
        .call("navigate", json!({"url":site.url("/leak")}))
        .await;
    assert_eq!(navigated["status"], "completed", "navigate: {navigated}");
    let listed = live
        .rig
        .tool("page_list", json!({"sessionId":live.session_id}))
        .await;
    for (what, result) in [("navigate", &navigated), ("page_list", &listed)] {
        assert!(
            !result.to_string().contains(BEARER_TOKEN),
            "{what} exposed the title's bearer token: {result}"
        );
    }
    let mut titles = Vec::new();
    strings_under(&navigated["evidence"], "title", &mut titles);
    assert!(
        titles.contains(&"[redacted]"),
        "navigate did not report the withheld title: {navigated}"
    );
    assert_eq!(
        listed_page(&live).await.1,
        "[redacted]",
        "page_list: {listed}"
    );
    live.close().await;
}
