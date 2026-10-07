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
