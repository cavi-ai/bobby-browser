//! Browser behavior cases on minimal local pages, run against every engine
//! through the production MCP server.

use serde_json::{json, Value};

use super::rig::{assert_node, find_node, strings_under, targets_under, Live, Rig};
use test_site::{FixtureSite, Route};

/// Invokes `$each! { case, ... }` with every case both engines run, then any
/// engine-specific `$extra` cases. Each engine's suite expands this one list.
#[allow(unused_macros)]
macro_rules! every_case {
    ($each:ident $(, $extra:ident)* $(,)?) => {
        $each! {
            hidden_subtrees_do_not_spend_the_node_budget,
            snapshot_target_scopes_the_tree,
            workflow_start_reports_the_settled_page,
            observe_after_navigate_includes_late_content,
            accessible_names_are_computed,
            secret_words_are_not_secrets,
            scheme_like_text_is_page_text,
            disclosed_credentials_are_withheld,
            large_dom_link_resolves_for_intent_follow,
            redacted_page_fields_do_not_block_actions,
            snapshot_budget_keeps_ancestors_of_kept_nodes,
            containers_are_not_named_from_content,
            type_text_reaches_the_visible_duplicate,
            intent_follow_clicks_the_visible_duplicate,
            snapshot_scopes_to_a_named_list,
            scoped_snapshot_targets_act_on_the_scoped_element,
            snapshot_scoped_behind_a_modal_reports_the_dialog,
            snapshot_targets_act_on_the_described_element,
            snapshot_targets_cover_widget_items,
            snapshot_target_types_into_a_slot_labelled_field,
            snapshot_targets_resolve_beside_a_modal_dialog,
            type_text_enter_reports_the_settled_page,
            type_text_types_into_a_field_that_takes_only_real_input,
            type_text_enter_accepts_a_reformatted_landed_field,
            intent_follow_post_state_shows_the_settled_page,
            type_text_enter_reports_the_rewritten_url,
            intent_follow_post_state_waits_for_fetched_content,
            intent_follow_waits_for_a_late_data_request,
            settles_beside_class_churn,
            settles_beside_text_churn,
            settles_beside_moving_children,
            settles_beside_combined_churn,
            settle_cap_trace_names_the_churn,
            network_tracking_survives_a_heavy_page,
            browser_events_survive_a_request_burst,
            type_text_enter_reports_a_late_title,
            type_text_enter_waits_for_a_landed_response,
            navigate_settles_on_a_polling_page,
            navigate_ignores_requests_the_navigation_cancelled,
            type_text_enter_reports_a_keydown_navigation_at_once,
            actions_wait_for_a_late_target,
            actions_fail_a_missing_target_within_one_bound,
            click_refuses_a_covered_target,
            hidden_state_holds_for_a_removed_control,
            intent_follow_reports_a_dialog_that_opened,
            dismiss_completes_when_a_same_named_control_appears,
            shadow_root_controls_act_from_snapshot_targets,
            navigate_waits_for_late_scripts,
            page_titles_withhold_disclosed_credentials,
            observation_carries_each_text_once,
            sign_in_fields_show_their_labels,
            concurrent_sessions_each_keep_their_own_page,
            $($extra,)*
        }
    };
}
#[allow(unused_imports)]
pub(crate) use every_case;

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

/// Hidden subtrees spend no node budget: the default `workflow_observe` and
/// a 120-node `a11y_snapshot` reach `main` and its list items.
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

/// An `a11y_snapshot` scoped to `{role: main}` holds no `banner`, and a scope
/// naming no region fails `targetNotFound`.
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

/// A page that redirects to a login page which replaces itself back: the
/// `workflow_start` result reports the final URL and title.
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

/// Content a script inserts after load is in the first `workflow_observe`
/// after `navigate`.
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

/// Controls named only by an svg `<title>`, a two-id `aria-labelledby`, a
/// placeholder, nested spans or an image `alt` get that name and act by it.
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

/// Words like "token", "credential" and "password" in page text are not
/// secret material: the page is observed and actions on it succeed.
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

/// Text that discloses a credential (a bearer header value, a PEM private
/// key) is never in a result.
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

/// `intent_follow` resolves a link that follows thousands of hidden and more
/// than 4096 visible nodes.
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

/// A target absent from a page larger than the Firefox candidate cap fails
/// `resourceExhausted` naming the truncation. Firefox only.
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

/// A page field that matches the secret rule is redacted, and actions on the
/// page proceed without exposing it.
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

/// A snapshot bounded by `maxNodes` keeps the ancestors of every node it
/// keeps and reports `truncated`.
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

/// Landmarks, lists and forms are not named from their content; list items
/// keep their text.
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

/// `type_text` reaches the visible textbox when a hidden duplicate shares its
/// identifying attribute, and reaches a textbox below the fold.
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

/// `intent_follow` clicks the visible link when hidden copies and a non-link
/// element share its identifying attribute.
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

/// A link in a list shares its name with a link before the list: a snapshot
/// scoped to the list gives a target that clicks the list's link.
pub async fn scoped_snapshot_targets_act_on_the_scoped_element(rig: &Rig) {
    let body = r#"<header><a href="/top" data-id="top">Home</a></header>
        <main><ul aria-label="Results"><li><a href="/item" data-id="item">Home</a></li></ul>
        <p role="status" aria-label="idle" id="status"></p></main>
        <script>
          document.addEventListener("click", (event) => {
            const reached = event.target.closest("[data-id]");
            if (reached) document.getElementById("status").setAttribute("aria-label", "acted " + reached.dataset.id);
            event.preventDefault();
          }, true);
        </script>"#;
    let site = FixtureSite::spawn(vec![("/links", Route::Html(page("Links", body)))]).await;
    let live = Live::open(rig, &site.url("/links")).await;
    let scoped = live
        .snapshot(json!({"target":{"role":"list","accessibleName":"Results"}}))
        .await;
    let mut targets = Vec::new();
    targets_under(&scoped, &mut targets);
    let link = targets
        .iter()
        .find(|(role, target)| *role == "link" && target["accessibleName"] == "Home")
        .map(|(_, target)| (*target).clone())
        .unwrap_or_else(|| panic!("no Home link target in the scoped snapshot: {scoped}"));
    let clicked = live.call("click", json!({"target":link})).await;
    assert_eq!(clicked["status"], "completed", "click {link}: {clicked}");
    let after = live.snapshot(json!({})).await;
    assert!(
        find_node(&after, "status", Some("acted item")).is_some(),
        "the scoped target {link} clicked another link: {after}"
    );
    live.close().await;
}

/// A snapshot scoped to a region an open modal dialog hides fails
/// targetObscured saying a modal dialog is in the way, not targetNotFound.
pub async fn snapshot_scoped_behind_a_modal_reports_the_dialog(rig: &Rig) {
    let body = r#"<main aria-hidden="true"><h1>Page</h1></main>
        <div role="dialog" aria-modal="true" aria-label="Notice"
             style="position:fixed;inset:0;background:#fff"><button>Close</button></div>"#;
    let site = FixtureSite::spawn(vec![("/modal", Route::Html(page("Modal", body)))]).await;
    let live = Live::open(rig, &site.url("/modal")).await;
    let scoped = live.snapshot(json!({"target":{"role":"main"}})).await;
    assert_eq!(
        scoped["error"]["code"], "targetObscured",
        "scoped snapshot behind a modal: {scoped}"
    );
    assert!(
        scoped["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("modal dialog")),
        "the error does not say a modal dialog is in the way: {scoped}"
    );
    live.close().await;
}

/// A snapshot scoped to a list named by `aria-label` returns that list, not
/// a hidden list of the same name.
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

/// A text field inside a shadow root, labelled through a slotted element
/// that carries its own name: the snapshot's target for the field types into it.
pub async fn snapshot_target_types_into_a_slot_labelled_field(rig: &Rig) {
    let body = r#"<main><search-field><span slot="scope" aria-label="Within this section"></span></search-field></main>
        <script>
          customElements.define("search-field", class extends HTMLElement {
            constructor() {
              super();
              this.attachShadow({mode: "open"}).innerHTML =
                '<label><slot name="scope"></slot><textarea rows="1" placeholder="Search"></textarea></label>';
            }
          });
        </script>"#;
    let site = FixtureSite::spawn(vec![("/field", Route::Html(page("Field", body)))]).await;
    let live = Live::open(rig, &site.url("/field")).await;
    let snapshot = live.snapshot(json!({})).await;
    let mut targets = Vec::new();
    targets_under(&snapshot, &mut targets);
    let field = targets
        .iter()
        .find(|(role, _)| *role == "textbox")
        .map(|(_, target)| (*target).clone())
        .unwrap_or_else(|| panic!("no textbox target in the snapshot: {snapshot}"));
    let typed = live
        .call("type_text", json!({"target":field,"value":"query"}))
        .await;
    assert_eq!(typed["status"], "completed", "type_text {field}: {typed}");
    let located = live
        .call(
            "intent_locate",
            json!({"purpose":"Find the search field",
                   "hints":{"role":field["role"],"accessibleName":field["accessibleName"]}}),
        )
        .await;
    assert_eq!(
        located["status"], "completed",
        "intent_locate {field}: {located}"
    );
    live.close().await;
}

/// A modal dialog over a page, hidden from assistive technology, that has a
/// control with the same role and name: the snapshot's targets for that name
/// resolve, and one reaches the dialog's.
pub async fn snapshot_targets_resolve_beside_a_modal_dialog(rig: &Rig) {
    let body = r#"<main aria-hidden="true"><button data-id="page">Close</button></main>
        <div role="dialog" aria-modal="true" aria-label="Notice"
             style="position:fixed;inset:0;background:#fff">
          <button data-id="dialog">Close</button>
          <p role="status" aria-label="idle" id="status"></p>
        </div>
        <script>
          document.addEventListener("click", (event) => {
            const reached = event.target.closest("[data-id]");
            if (reached) document.getElementById("status").setAttribute("aria-label", "acted " + reached.dataset.id);
          }, true);
        </script>"#;
    let site = FixtureSite::spawn(vec![("/modal", Route::Html(page("Modal", body)))]).await;
    let live = Live::open(rig, &site.url("/modal")).await;
    let snapshot = live.snapshot(json!({})).await;
    let mut targets = Vec::new();
    targets_under(&snapshot, &mut targets);
    let closes: Vec<Value> = targets
        .iter()
        .filter(|(role, target)| *role == "button" && target["accessibleName"] == "Close")
        .map(|(_, target)| (*target).clone())
        .collect();
    assert!(
        !closes.is_empty(),
        "no Close target in the snapshot: {snapshot}"
    );
    let mut reached_dialog = false;
    for target in closes {
        let clicked = live.call("click", json!({"target":target})).await;
        let code = clicked["error"]["code"].as_str().unwrap_or_default();
        assert!(
            code != "targetAmbiguous" && code != "targetNotFound",
            "snapshot target {target} did not resolve: {clicked}"
        );
        let after = live.snapshot(json!({})).await;
        reached_dialog |= find_node(&after, "status", Some("acted dialog")).is_some();
    }
    assert!(
        reached_dialog,
        "no Close target reached the dialog's button"
    );
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

/// A text area with no id keeps its own value, taken only from real typing,
/// and submits on Enter: type_text types into it and Enter submits.
pub async fn type_text_types_into_a_field_that_takes_only_real_input(rig: &Rig) {
    let body = r#"<main><textarea rows="1" aria-label="Query"></textarea>
        <p role="status" aria-label="idle" id="status"></p></main>
        <script>
          const area = document.querySelector("textarea");
          const status = document.getElementById("status");
          let typed = "";
          area.addEventListener("input", (event) => {
            if (event instanceof InputEvent) typed = area.value;
            area.value = typed;
          });
          area.addEventListener("keydown", (event) => {
            if (event.key !== "Enter") return;
            event.preventDefault();
            status.setAttribute("aria-label", "submitted " + typed);
          });
        </script>"#;
    let site = FixtureSite::spawn(vec![("/field", Route::Html(page("Field", body)))]).await;
    let live = Live::open(rig, &site.url("/field")).await;
    let typed = live
        .call(
            "type_text",
            json!({"target":{"role":"textbox","accessibleName":"Query"},"value":"query\n"}),
        )
        .await;
    assert_eq!(typed["status"], "completed", "type_text: {typed}");
    let after = live.snapshot(json!({})).await;
    assert!(
        find_node(&after, "status", Some("submitted query")).is_some(),
        "the field did not take the typed text and Enter: {after}"
    );
    live.close().await;
}

/// Enter in a single-line input that pushState-navigates to a URL with a
/// query and renders later: type_text evidence reports only the settled URL
/// and title, the ones page_list shows.
pub async fn type_text_enter_reports_the_settled_page(rig: &Rig) {
    let home = page(
        "Home",
        r#"<header><input id="q" aria-label="Search"></header><main id="main"><h1>Home</h1></main>
        <script>
            document.getElementById("q").addEventListener("keydown", (event) => {
              if (event.key !== "Enter") return;
              event.preventDefault();
              const value = event.target.value;
              history.pushState({}, "", "/results?q=" + encodeURIComponent(value) + "&from=header");
              const main = document.getElementById("main");
              main.innerHTML = "<p id='tick'>loading</p>";
              let ticks = 0;
              const loader = setInterval(() => {
                document.getElementById("tick").textContent = "loading " + ++ticks;
                if (ticks < 25) return;
                clearInterval(loader);
                document.title = "Results";
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
                   "value":"query terms\n","clearFirst":true}),
        )
        .await;
    assert_eq!(typed["status"], "completed", "type_text: {typed}");
    let mut navigations = Vec::new();
    objects_of_kind(&typed, "navigation", &mut navigations);
    let expected_url = site.url("/results?q=query%20terms&from=header");
    assert!(
        navigations
            .iter()
            .any(|item| item["url"] == expected_url.as_str() && item["title"] == "Results"),
        "type_text did not report the settled page {expected_url} titled \"Results\": {typed}"
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

/// Enter submits a GET form and the landed page shows the query, reformatted,
/// in its own search box: type_text completes and reports the landed page.
pub async fn type_text_enter_accepts_a_reformatted_landed_field(rig: &Rig) {
    let search = r#"<form action="/results"><input name="q" aria-label="Search"></form>"#;
    let home = page("Home", &format!("{search}<main><h1>Home</h1></main>"));
    let results = page(
        "Results",
        &format!(
            r#"{search}<main><h1>Results</h1></main>
            <script>
                const query = new URLSearchParams(location.search).get("q") || "";
                document.querySelector("input").value =
                    query.replace(/\b\w/g, (letter) => letter.toUpperCase());
            </script>"#
        ),
    );
    let site = FixtureSite::spawn(vec![
        ("/home", Route::Html(home)),
        ("/results", Route::Html(results)),
    ])
    .await;
    let live = Live::open(rig, &site.url("/home")).await;
    let typed = live
        .call(
            "type_text",
            json!({"target":{"role":"textbox","accessibleName":"Search"},
                   "value":"query terms\n","clearFirst":true}),
        )
        .await;
    assert_eq!(typed["status"], "completed", "type_text: {typed}");
    let mut navigations = Vec::new();
    objects_of_kind(&typed, "navigation", &mut navigations);
    let expected_url = site.url("/results?q=query+terms");
    assert!(
        navigations
            .iter()
            .any(|item| item["url"] == expected_url.as_str() && item["title"] == "Results"),
        "type_text did not report the landed page {expected_url}: {typed}"
    );
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

/// A link that pushState-navigates and renders its content in later ticks:
/// intent_follow completes once the page settles, so postState is rendered.
pub async fn intent_follow_post_state_shows_the_settled_page(rig: &Rig) {
    let start = page(
        "Start",
        r#"<main id="main"><a id="go" href="/items/list">Item list</a></main>
        <script>
            document.getElementById("go").addEventListener("click", (event) => {
              event.preventDefault();
              history.pushState({}, "", "/items/list");
              const main = document.getElementById("main");
              main.innerHTML = "<div role='presentation'></div>".repeat(19);
              let ticks = 0;
              const loader = setInterval(() => {
                main.firstChild.setAttribute("data-tick", String(++ticks));
                if (ticks < 15) return;
                clearInterval(loader);
                document.title = "Item list";
                main.innerHTML = "<h1>Item list</h1><ul><li><a href='/items/1'>Item 1</a></li></ul>";
              }, 100);
            });
        </script>"#,
    );
    let site = FixtureSite::spawn(vec![("/start", Route::Html(start))]).await;
    let live = Live::open(rig, &site.url("/start")).await;
    let followed = live
        .call(
            "intent_follow",
            json!({
                "purpose":"Open the item list",
                "hints":{"role":"link","accessibleName":"Item list"},
                "expectedState":{
                    "condition":{"kind":"url","matcher":{"kind":"contains","value":"/items/"}},
                    "timeoutMs":15000
                }
            }),
        )
        .await;
    assert_eq!(followed["status"], "completed", "intent_follow: {followed}");
    assert!(
        find_node(&followed["postState"], "heading", Some("Item list")).is_some(),
        "intent_follow postState is not the rendered page: {followed}"
    );
    live.close().await;
}

/// Enter pushState-navigates to a URL with a query, replaceState adds a
/// parameter once the results render, the DOM never stops changing, and the
/// page is busy past the settle cap: type_text reports the rewritten URL and
/// the rendered title.
pub async fn type_text_enter_reports_the_rewritten_url(rig: &Rig) {
    let home = page(
        "Home",
        r#"<header><input id="q" aria-label="Search"></header><main id="main"><h1>Home</h1></main>
        <script>
            document.getElementById("q").addEventListener("keydown", (event) => {
              if (event.key !== "Enter") return;
              event.preventDefault();
              const path = "/results/all/?q=" + encodeURIComponent(event.target.value) + "&from=header";
              history.pushState({}, "", path);
              const main = document.getElementById("main");
              main.innerHTML = "<p id='clock'>0</p>";
              let ticks = 0;
              setInterval(() => {
                document.getElementById("clock").textContent = String(++ticks);
              }, 100);
              setTimeout(() => {
                history.replaceState({}, "", path + "&ref=a1");
                document.title = "Results";
                main.insertAdjacentHTML("beforeend", "<h1>Results</h1>");
              }, 1500);
              setTimeout(() => {
                const end = Date.now() + 4000;
                while (Date.now() < end) {}
              }, 4500);
            });
        </script>"#,
    );
    let site = FixtureSite::spawn(vec![("/home", Route::Html(home))]).await;
    let live = Live::open(rig, &site.url("/home")).await;
    let typed = live
        .call(
            "type_text",
            json!({"target":{"role":"textbox","accessibleName":"Search"},
                   "value":"query terms\n","clearFirst":true}),
        )
        .await;
    assert_eq!(typed["status"], "completed", "type_text: {typed}");
    let mut navigations = Vec::new();
    objects_of_kind(&typed, "navigation", &mut navigations);
    let expected_url = site.url("/results/all/?q=query%20terms&from=header&ref=a1");
    assert!(
        navigations
            .iter()
            .any(|item| item["url"] == expected_url.as_str() && item["title"] == "Results"),
        "type_text did not report {expected_url} titled \"Results\": {typed}"
    );
    live.close().await;
}

/// A link pushState-navigates, renders an empty skeleton, and fills it from a
/// fetch that outlasts the quiet window: intent_follow's postState shows the
/// fetched content.
pub async fn intent_follow_post_state_waits_for_fetched_content(rig: &Rig) {
    let start = page(
        "Start",
        r#"<main id="main"><a id="go" href="/listings/search?kw=all">Open listings</a></main>
        <script>
            document.getElementById("go").addEventListener("click", (event) => {
              event.preventDefault();
              history.pushState({}, "", "/listings/search?kw=all&from=nav");
              const main = document.getElementById("main");
              main.innerHTML = "<div role='presentation'></div>".repeat(19);
              fetch("/api/listings")
                .then((response) => response.json())
                .then((names) => {
                  document.title = "Listings";
                  main.innerHTML = "<h1>Listings</h1><ul>" +
                    names.map((name) => "<li>" + name + "</li>").join("") + "</ul>";
                });
            });
        </script>"#,
    );
    let site = FixtureSite::spawn(vec![
        ("/start", Route::Html(start)),
        (
            "/api/listings",
            Route::Delayed {
                delay: std::time::Duration::from_millis(1_500),
                content_type: "application/json",
                body: r#"["First listing","Second listing"]"#.to_owned(),
            },
        ),
    ])
    .await;
    let live = Live::open(rig, &site.url("/start")).await;
    let followed = live
        .call(
            "intent_follow",
            json!({
                "purpose":"Open the listings",
                "hints":{"role":"link","accessibleName":"Open listings"},
                "expectedState":{
                    "condition":{"kind":"url","matcher":{"kind":"contains","value":"/listings/"}},
                    "timeoutMs":15000
                }
            }),
        )
        .await;
    assert_eq!(followed["status"], "completed", "intent_follow: {followed}");
    assert!(
        find_node(&followed["postState"], "heading", Some("Listings")).is_some(),
        "intent_follow postState is not the fetched page: {followed}"
    );
    live.close().await;
}

/// Runs of each timing-dependent case; every run must pass.
const SETTLE_RUNS: usize = 20;

/// The "Open results" link pushState-navigates and renders a skeleton into
/// `#main`, then starts its data request 0-800 ms later; the request takes
/// 600-1500 ms.
const LATE_DATA_SCRIPT: &str = r#"<script>
    document.getElementById("go").addEventListener("click", (event) => {
      event.preventDefault();
      history.pushState({}, "", "/results/list");
      const main = document.getElementById("main");
      main.innerHTML = "<div role='presentation'></div>".repeat(19);
      setTimeout(() => {
        fetch("/api/rows?ms=" + (600 + Math.floor(Math.random() * 901)))
          .then((response) => response.json())
          .then((rows) => {
            main.innerHTML = "<h1>Results</h1><ul>" +
              rows.map((row) => "<li>" + row + "</li>").join("") + "</ul>";
          });
      }, Math.floor(Math.random() * 801));
    });
</script>"#;

/// The data request [`LATE_DATA_SCRIPT`] makes.
fn rows_route() -> (&'static str, Route) {
    (
        "/api/rows",
        Route::QueryDelayed {
            content_type: "application/json",
            body: r#"["First row","Second row"]"#.to_owned(),
        },
    )
}

/// A start page with the late-data link.
fn late_data_routes() -> Vec<(&'static str, Route)> {
    let start = page(
        "Start",
        &format!(
            r#"<main id="main"><a id="go" href="/results/list">Open results</a></main>
        {LATE_DATA_SCRIPT}"#
        ),
    );
    vec![("/start", Route::Html(start)), rows_route()]
}

/// Loads the late-data start page and follows its link. `None` when
/// intent_follow's postState shows the fetched content.
async fn follow_late_data_link(live: &Live<'_>, site: &FixtureSite) -> Option<String> {
    let loaded = live
        .call("navigate", json!({"url":site.url("/start")}))
        .await;
    assert_eq!(loaded["status"], "completed", "navigate: {loaded}");
    follow_results_link(live).await
}

/// Follows the late-data link on the current page. `None` when
/// intent_follow's postState shows the fetched content.
async fn follow_results_link(live: &Live<'_>) -> Option<String> {
    let followed = live
        .call(
            "intent_follow",
            json!({
                "purpose":"Open the results",
                "hints":{"role":"link","accessibleName":"Open results"},
                "expectedState":{
                    "condition":{"kind":"url","matcher":{"kind":"contains","value":"/results/"}},
                    "timeoutMs":15000
                }
            }),
        )
        .await;
    if followed["status"] != "completed" {
        Some(format!("status {}", followed["status"]))
    } else if find_node(&followed["postState"], "heading", Some("Results")).is_none() {
        Some("postState is not the fetched page".to_owned())
    } else {
        None
    }
}

/// Every intent_follow on the late-data start page shows the fetched content.
pub async fn intent_follow_waits_for_a_late_data_request(rig: &Rig) {
    let site = FixtureSite::spawn(late_data_routes()).await;
    let live = Live::open(rig, &site.url("/start")).await;
    let mut failures = Vec::new();
    for run in 1..=SETTLE_RUNS {
        if let Some(failure) = follow_late_data_link(&live, &site).await {
            failures.push(format!("run {run}: {failure}"));
        }
    }
    live.close().await;
    assert!(
        failures.is_empty(),
        "{} of {SETTLE_RUNS} runs failed: {failures:?}",
        failures.len()
    );
}

/// Placeholders that toggle a class every 100 ms.
const SHIMMER: &str = r#"<div class="placeholder"></div><div class="placeholder"></div>
<script>
  setInterval(() => {
    for (const node of document.querySelectorAll(".placeholder")) node.classList.toggle("dim");
  }, 100);
</script>"#;
/// A counter whose text changes every 200 ms.
const COUNTER: &str = r#"<p>Viewers <span id="count">0</span></p>
<script>
  {
    const count = document.getElementById("count").firstChild;
    let value = 0;
    setInterval(() => { count.data = String(++value); }, 200);
  }
</script>"#;
/// A carousel that moves its first slide to the end every second.
const CAROUSEL: &str = r#"<ul id="slides"><li>Slide one</li><li>Slide two</li><li>Slide three</li></ul>
<script>
  {
    const slides = document.getElementById("slides");
    setInterval(() => slides.appendChild(slides.firstElementChild), 1000);
  }
</script>"#;

/// Runs of each churn case; every run must pass.
const CHURN_RUNS: usize = 10;

/// The `exit` of each settle logged after the first `seen` captured events.
fn settle_exits(
    capture: Option<&observability::test_support::CaptureSink>,
    seen: usize,
) -> Vec<String> {
    capture.map_or_else(Vec::new, |capture| {
        capture.events()[seen..]
            .iter()
            .filter(|event| event["fields"]["message"] == "navigation settle")
            .map(|event| event["fields"]["exit"].as_str().unwrap_or("").to_owned())
            .collect()
    })
}

/// A page with the late-data link next to `widgets`, which never stop
/// changing the DOM. Every navigate to it settles quiet within 2.5 s; every
/// intent_follow of its link settles quiet and its postState shows the
/// fetched content.
async fn settles_beside_churn(rig: &Rig, widgets: &str) {
    let feed = page(
        "Feed",
        &format!(
            r#"<main><div id="main"><h1>Feed</h1><a id="go" href="/results/list">Open results</a></div>
        {widgets}</main>{LATE_DATA_SCRIPT}"#
        ),
    );
    let site = FixtureSite::spawn(vec![("/feed", Route::Html(feed)), rows_route()]).await;
    // `RUST_LOG` installs the stdio subscriber instead; settle exits then go unchecked.
    let capture = std::env::var_os("RUST_LOG")
        .is_none()
        .then(observability::test_support::CaptureSink::install);
    let live = Live::open(rig, &site.url("/feed")).await;
    let quiet = |exits: &[String]| {
        capture.is_none() || (!exits.is_empty() && exits.iter().all(|exit| exit == "quiet"))
    };
    let mut times = Vec::new();
    let mut failures = Vec::new();
    for run in 1..=CHURN_RUNS {
        let seen = capture.as_ref().map_or(0, |capture| capture.events().len());
        let started = std::time::Instant::now();
        let loaded = live
            .call("navigate", json!({"url":site.url("/feed")}))
            .await;
        let navigated = started.elapsed();
        assert_eq!(loaded["status"], "completed", "navigate: {loaded}");
        let navigate_exits = settle_exits(capture.as_ref(), seen);
        let seen = capture.as_ref().map_or(0, |capture| capture.events().len());
        let started = std::time::Instant::now();
        let followed = follow_results_link(&live).await;
        let follow = started.elapsed();
        let follow_exits = settle_exits(capture.as_ref(), seen);
        times.push((navigated.as_millis(), follow.as_millis()));
        if navigated > std::time::Duration::from_millis(2_500) || !quiet(&navigate_exits) {
            failures.push(format!(
                "run {run}: navigate took {} ms, settle exits {navigate_exits:?}",
                navigated.as_millis()
            ));
        }
        if followed.is_some() || follow > std::time::Duration::from_secs(5) || !quiet(&follow_exits)
        {
            failures.push(format!(
                "run {run}: intent_follow took {} ms, settle exits {follow_exits:?}: {followed:?}",
                follow.as_millis()
            ));
        }
    }
    live.close().await;
    eprintln!("settle beside churn, (navigate ms, intent_follow ms): {times:?}");
    assert!(
        failures.is_empty(),
        "{} failures in {CHURN_RUNS} runs: {failures:?}",
        failures.len()
    );
}

pub async fn settles_beside_class_churn(rig: &Rig) {
    settles_beside_churn(rig, SHIMMER).await;
}

pub async fn settles_beside_text_churn(rig: &Rig) {
    settles_beside_churn(rig, COUNTER).await;
}

pub async fn settles_beside_moving_children(rig: &Rig) {
    settles_beside_churn(rig, CAROUSEL).await;
}

pub async fn settles_beside_combined_churn(rig: &Rig) {
    settles_beside_churn(rig, &[SHIMMER, COUNTER, CAROUSEL].concat()).await;
}

/// A bar inside `main` rewrites its `data-progress` attribute every 50 ms, so
/// navigating to it settles at the cap. That settle's trace names
/// `data-progress` with a nonzero count and places the churn inside `main`.
pub async fn settle_cap_trace_names_the_churn(rig: &Rig) {
    if std::env::var_os("RUST_LOG").is_some() {
        eprintln!("settle_cap_trace_names_the_churn skipped: RUST_LOG replaces the capture sink");
        return;
    }
    let progress = page(
        "Progress",
        r#"<main><h1>Progress</h1><div id="bar" data-progress="0"></div></main>
        <script>
            {
                const bar = document.getElementById("bar");
                let value = 0;
                setInterval(() => bar.setAttribute("data-progress", String(++value)), 50);
            }
        </script>"#,
    );
    let site = FixtureSite::spawn(vec![("/progress", Route::Html(progress))]).await;
    let capture = observability::test_support::CaptureSink::install();
    let live = Live::open(rig, &site.url("/progress")).await;
    let seen = capture.events().len();
    let loaded = live
        .call("navigate", json!({"url":site.url("/progress")}))
        .await;
    live.close().await;
    assert_eq!(loaded["status"], "completed", "navigate: {loaded}");
    let settles: Vec<Value> = capture.events()[seen..]
        .iter()
        .filter(|event| event["fields"]["message"] == "navigation settle")
        .cloned()
        .collect();
    assert!(
        !settles.is_empty() && settles.iter().all(|event| event["fields"]["exit"] == "cap"),
        "every settle ends at the cap: {settles:?}"
    );
    for event in &settles {
        let fields = &event["fields"];
        let progress = fields["top_attributes"]
            .as_str()
            .unwrap_or_default()
            .split(' ')
            .find_map(|pair| pair.strip_prefix("data-progress="))
            .and_then(|count| count.parse::<u64>().ok());
        assert!(
            progress.is_some_and(|count| count > 0),
            "cap trace names data-progress: {event}"
        );
        assert_eq!(
            fields["inside_main"], true,
            "cap trace places the churn in main: {event}"
        );
    }
    eprintln!("settle cap trace: {settles:?}");
}

/// Requests the heavy page fires at once, more than a tracker holds.
const HEAVY_REQUESTS: usize = 5_000;
/// Late-data navigations after the heavy page.
const RUNS_AFTER_HEAVY_PAGE: usize = 10;

/// A page fires 5,000 requests and one with a 20 KB URL; the same session
/// then follows the late-data link ten times. Network tracking is never lost
/// and every intent_follow's postState shows the fetched content.
pub async fn network_tracking_survives_a_heavy_page(rig: &Rig) {
    let heavy = page(
        "Heavy",
        &format!(
            r#"<main><h1>Heavy</h1></main>
        <script>
            addEventListener("load", async () => {{
              fetch("/api/item?pad=" + "a".repeat(20 * 1024));
              for (let index = 0; index < {HEAVY_REQUESTS}; index += 100) {{
                const batch = [];
                for (let item = index; item < index + 100; item++) batch.push(fetch("/api/item?i=" + item));
                await Promise.all(batch);
              }}
            }});
        </script>"#
        ),
    );
    let mut routes = late_data_routes();
    routes.push(("/heavy", Route::Html(heavy)));
    routes.push((
        "/api/item",
        Route::Raw {
            content_type: "application/json",
            body: "{}".to_owned(),
        },
    ));
    let site = FixtureSite::spawn(routes).await;
    // `RUST_LOG` installs the stdio subscriber instead; tracking losses then go unchecked.
    let capture = std::env::var_os("RUST_LOG")
        .is_none()
        .then(observability::test_support::CaptureSink::install);
    let live = Live::open(rig, &site.url("/heavy")).await;
    let wait = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while site.hits("/api/item") <= HEAVY_REQUESTS {
        assert!(
            std::time::Instant::now() < wait,
            "the heavy page sent {} of {} requests",
            site.hits("/api/item"),
            HEAVY_REQUESTS + 1
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let mut failures = Vec::new();
    for run in 1..=RUNS_AFTER_HEAVY_PAGE {
        if let Some(failure) = follow_late_data_link(&live, &site).await {
            failures.push(format!("run {run}: {failure}"));
        }
    }
    let losses: Vec<String> = capture.as_ref().map_or_else(Vec::new, |capture| {
        capture
            .events()
            .iter()
            .filter(|event| {
                event["fields"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("tracking lost"))
            })
            .map(|event| event["fields"].to_string())
            .collect()
    });
    live.close().await;
    assert!(
        failures.is_empty() && losses.is_empty(),
        "{} of {RUNS_AFTER_HEAVY_PAGE} runs failed: {failures:?}; tracking losses: {losses:?}",
        failures.len()
    );
}

/// Requests the burst page fires.
const BURST_REQUESTS: usize = 5_000;
/// Fetches the burst page keeps in flight. Chromium refuses a page's fetches
/// past roughly 1,350 in flight.
const BURST_IN_FLIGHT: usize = 1_000;

/// Navigate settles while a page fires 5,000 logged fetches, 1,000 in flight;
/// no browser event is dropped and the next intent_follow's postState shows the
/// fetched content.
pub async fn browser_events_survive_a_request_burst(rig: &Rig) {
    let burst = page(
        "Burst",
        &format!(
            r#"<main><h1>Burst</h1><p id="rejected">rejected 0</p></main>
        <script>
            let next = 0;
            let rejected = 0;
            let firstError = "";
            const send = () => {{
              if (next >= {BURST_REQUESTS}) return;
              const index = next++;
              fetch("/api/burst?i=" + index)
                .then((response) => console.log("item", index, response.status))
                .catch((error) => {{
                  rejected += 1;
                  firstError = firstError || String(error);
                  document.getElementById("rejected").textContent =
                    "rejected " + rejected + ": " + firstError;
                }})
                .finally(send);
            }};
            for (let lane = 0; lane < {BURST_IN_FLIGHT}; lane++) send();
        </script>"#
        ),
    );
    let mut routes = late_data_routes();
    routes.push(("/burst", Route::Html(burst)));
    routes.push((
        "/api/burst",
        Route::Raw {
            content_type: "application/json",
            body: "{}".to_owned(),
        },
    ));
    let site = FixtureSite::spawn(routes).await;
    // `RUST_LOG` installs the stdio subscriber instead; dropped events then go unchecked.
    let capture = std::env::var_os("RUST_LOG")
        .is_none()
        .then(observability::test_support::CaptureSink::install);
    let live = Live::open(rig, &site.url("/start")).await;
    let loaded = live
        .call("navigate", json!({"url":site.url("/burst")}))
        .await;
    assert_eq!(loaded["status"], "completed", "navigate: {loaded}");
    let wait = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while site.hits("/api/burst") < BURST_REQUESTS {
        if std::time::Instant::now() >= wait {
            // The page counts the fetches the browser itself refused.
            let mut texts = Vec::new();
            let snapshot = live.snapshot(json!({})).await;
            strings_under(&snapshot, "name", &mut texts);
            panic!(
                "the burst page sent {} of {BURST_REQUESTS} requests; page reports {:?}",
                site.hits("/api/burst"),
                texts.iter().find(|text| text.starts_with("rejected"))
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let followed = follow_late_data_link(&live, &site).await;
    let dropped: Vec<String> = capture.as_ref().map_or_else(Vec::new, |capture| {
        capture
            .events()
            .iter()
            .filter(|event| {
                let fields = &event["fields"];
                let message = fields["message"].as_str().unwrap_or_default();
                (message.contains("tracking lost") && fields["reason"] == "lagged")
                    || message.contains("event stream lost events")
            })
            .map(|event| event["fields"].to_string())
            .collect()
    });
    live.close().await;
    assert!(
        followed.is_none() && dropped.is_empty(),
        "intent_follow: {followed:?}; dropped events: {dropped:?}"
    );
}

/// Enter pushState-navigates and fetches the results (600-1500 ms), renders
/// them, and sets the title 0-800 ms later. Every type_text reports the new
/// title.
pub async fn type_text_enter_reports_a_late_title(rig: &Rig) {
    let home = page(
        "Home",
        r#"<header><input id="q" aria-label="Search"></header><main id="main"><h1>Home</h1></main>
        <script>
            document.getElementById("q").addEventListener("keydown", (event) => {
              if (event.key !== "Enter") return;
              event.preventDefault();
              history.pushState({}, "", "/search?q=" + encodeURIComponent(event.target.value));
              const main = document.getElementById("main");
              main.innerHTML = "<p>Loading</p>";
              fetch("/api/results?ms=" + (600 + Math.floor(Math.random() * 901)))
                .then((response) => response.json())
                .then((rows) => {
                  main.innerHTML = "<h1>Search results</h1><ul>" +
                    rows.map((row) => "<li>" + row + "</li>").join("") + "</ul>";
                  setTimeout(() => {
                    document.title = "Search results";
                  }, Math.floor(Math.random() * 801));
                });
            });
        </script>"#,
    );
    let site = FixtureSite::spawn(vec![
        ("/home", Route::Html(home)),
        (
            "/api/results",
            Route::QueryDelayed {
                content_type: "application/json",
                body: r#"["First result","Second result"]"#.to_owned(),
            },
        ),
    ])
    .await;
    let live = Live::open(rig, &site.url("/home")).await;
    let expected_url = site.url("/search?q=query%20terms");
    let mut failures = Vec::new();
    for run in 1..=SETTLE_RUNS {
        let loaded = live
            .call("navigate", json!({"url":site.url("/home")}))
            .await;
        assert_eq!(loaded["status"], "completed", "navigate: {loaded}");
        let typed = live
            .call(
                "type_text",
                json!({"target":{"role":"textbox","accessibleName":"Search"},
                       "value":"query terms\n","clearFirst":true}),
            )
            .await;
        let mut navigations = Vec::new();
        objects_of_kind(&typed, "navigation", &mut navigations);
        if typed["status"] != "completed" {
            failures.push(format!("run {run}: status {}", typed["status"]));
        } else if !navigations
            .iter()
            .any(|item| item["url"] == expected_url.as_str() && item["title"] == "Search results")
        {
            let reported: Vec<_> = navigations.iter().map(|item| &item["title"]).collect();
            failures.push(format!("run {run}: reported titles {reported:?}"));
        }
    }
    live.close().await;
    assert!(
        failures.is_empty(),
        "{} of {SETTLE_RUNS} runs failed: {failures:?}",
        failures.len()
    );
}

/// Enter pushState-navigates, the results request lands inside the quiet
/// window, and the page renders 300 ms after it lands. Every type_text reports
/// the rendered title.
pub async fn type_text_enter_waits_for_a_landed_response(rig: &Rig) {
    let home = page(
        "Home",
        r#"<header><input id="q" aria-label="Search"></header><main id="main"><h1>Home</h1></main>
        <script>
            document.getElementById("q").addEventListener("keydown", (event) => {
              if (event.key !== "Enter") return;
              event.preventDefault();
              history.pushState({}, "", "/search?q=" + encodeURIComponent(event.target.value));
              const main = document.getElementById("main");
              main.innerHTML = "<p>Loading</p>";
              setTimeout(() => { main.innerHTML = "<p>Still loading</p>"; }, 400);
              fetch("/api/results?ms=1250")
                .then((response) => response.json())
                .then((rows) => setTimeout(() => {
                  document.title = "Search results";
                  main.innerHTML = "<h1>Search results</h1><ul>" +
                    rows.map((row) => "<li>" + row + "</li>").join("") + "</ul>";
                }, 300));
            });
        </script>"#,
    );
    let site = FixtureSite::spawn(vec![
        ("/home", Route::Html(home)),
        (
            "/api/results",
            Route::QueryDelayed {
                content_type: "application/json",
                body: r#"["First result","Second result"]"#.to_owned(),
            },
        ),
    ])
    .await;
    let live = Live::open(rig, &site.url("/home")).await;
    let expected_url = site.url("/search?q=query%20terms");
    let mut failures = Vec::new();
    for run in 1..=LANDED_RUNS {
        let loaded = live
            .call("navigate", json!({"url":site.url("/home")}))
            .await;
        assert_eq!(loaded["status"], "completed", "navigate: {loaded}");
        let typed = live
            .call(
                "type_text",
                json!({"target":{"role":"textbox","accessibleName":"Search"},
                       "value":"query terms\n","clearFirst":true}),
            )
            .await;
        let mut navigations = Vec::new();
        objects_of_kind(&typed, "navigation", &mut navigations);
        if typed["status"] != "completed" {
            failures.push(format!("run {run}: status {}", typed["status"]));
        } else if !navigations
            .iter()
            .any(|item| item["url"] == expected_url.as_str() && item["title"] == "Search results")
        {
            let reported: Vec<_> = navigations.iter().map(|item| &item["title"]).collect();
            failures.push(format!("run {run}: reported titles {reported:?}"));
        }
    }
    live.close().await;
    assert!(
        failures.is_empty(),
        "{} of {LANDED_RUNS} runs failed: {failures:?}",
        failures.len()
    );
}

const LANDED_RUNS: usize = 5;

/// A page polls a fetch every 300 ms without changing the document. Every
/// navigate to it settles within 2.5 s.
pub async fn navigate_settles_on_a_polling_page(rig: &Rig) {
    let polling = page(
        "Polling",
        r#"<main><h1>Polling</h1></main>
        <script>
            setInterval(() => { fetch("/api/poll?ms=20"); }, 300);
        </script>"#,
    );
    let site = FixtureSite::spawn(vec![
        ("/polling", Route::Html(polling)),
        (
            "/api/poll",
            Route::QueryDelayed {
                content_type: "application/json",
                body: "[]".to_owned(),
            },
        ),
    ])
    .await;
    let live = Live::open(rig, &site.url("/polling")).await;
    let mut failures = Vec::new();
    for run in 1..=LANDED_RUNS {
        let started = std::time::Instant::now();
        let loaded = live
            .call("navigate", json!({"url":site.url("/polling")}))
            .await;
        let elapsed = started.elapsed();
        assert_eq!(loaded["status"], "completed", "navigate: {loaded}");
        if elapsed > std::time::Duration::from_millis(2_500) {
            failures.push(format!(
                "run {run}: navigate took {} ms",
                elapsed.as_millis()
            ));
        }
    }
    live.close().await;
    assert!(
        failures.is_empty(),
        "{} of {LANDED_RUNS} runs failed: {failures:?}",
        failures.len()
    );
}

/// A page starts a 2-3 s data request after it settles, and the agent
/// navigates to a static page while that request is in flight. Every navigate
/// settles quiet within 2 s.
pub async fn navigate_ignores_requests_the_navigation_cancelled(rig: &Rig) {
    let source = page(
        "Source",
        r#"<main><h1>Source</h1></main>
        <script>
            addEventListener("load", () => setTimeout(() => {
              fetch("/api/slow?ms=" + (2000 + Math.floor(Math.random() * 1001)));
            }, 800));
        </script>"#,
    );
    let site = FixtureSite::spawn(vec![
        ("/source", Route::Html(source)),
        (
            "/target",
            Route::Html(page("Target", "<main><h1>Target</h1></main>")),
        ),
        (
            "/api/slow",
            Route::QueryDelayed {
                content_type: "application/json",
                body: "[]".to_owned(),
            },
        ),
    ])
    .await;
    let live = Live::open(rig, &site.url("/target")).await;
    // `RUST_LOG` installs the stdio subscriber instead; settle exits then go unchecked.
    let capture = std::env::var_os("RUST_LOG")
        .is_none()
        .then(observability::test_support::CaptureSink::install);
    let mut failures = Vec::new();
    for run in 1..=SETTLE_RUNS {
        let requests = site.hits("/api/slow");
        let loaded = live
            .call("navigate", json!({"url":site.url("/source")}))
            .await;
        assert_eq!(loaded["status"], "completed", "navigate: {loaded}");
        let wait = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while site.hits("/api/slow") == requests {
            assert!(
                std::time::Instant::now() < wait,
                "run {run}: the data request never started"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let seen = capture.as_ref().map_or(0, |capture| capture.events().len());
        let started = std::time::Instant::now();
        let moved = live
            .call("navigate", json!({"url":site.url("/target")}))
            .await;
        let elapsed = started.elapsed();
        assert_eq!(moved["status"], "completed", "navigate: {moved}");
        let exits: Vec<String> = capture.as_ref().map_or_else(Vec::new, |capture| {
            capture.events()[seen..]
                .iter()
                .filter(|event| event["fields"]["message"] == "navigation settle")
                .map(|event| event["fields"]["exit"].as_str().unwrap_or("").to_owned())
                .collect()
        });
        let exits_quiet =
            capture.is_none() || (!exits.is_empty() && exits.iter().all(|exit| exit == "quiet"));
        if elapsed > std::time::Duration::from_secs(2) || !exits_quiet {
            failures.push(format!(
                "run {run}: navigate took {} ms, settle exits {exits:?}",
                elapsed.as_millis()
            ));
        }
    }
    live.close().await;
    assert!(
        failures.is_empty(),
        "{} of {SETTLE_RUNS} runs failed: {failures:?}",
        failures.len()
    );
}

/// Enter pushState-navigates inside its own keydown and renders the results:
/// every type_text reports the pushed URL and title within the settle window.
pub async fn type_text_enter_reports_a_keydown_navigation_at_once(rig: &Rig) {
    let home = page(
        "Home",
        r#"<header><input id="q" aria-label="Search"></header><main id="main"><h1>Home</h1></main>
        <script>
            document.getElementById("q").addEventListener("keydown", (event) => {
              if (event.key !== "Enter") return;
              event.preventDefault();
              history.pushState({}, "", "/search?q=" + encodeURIComponent(event.target.value) +
                "&at=" + Date.now());
              document.title = "Search results";
              document.getElementById("main").innerHTML =
                "<h1>Search results</h1><ul><li>First result</li><li>Second result</li></ul>";
            });
        </script>"#,
    );
    let site = FixtureSite::spawn(vec![("/home", Route::Html(home))]).await;
    let live = Live::open(rig, &site.url("/home")).await;
    let pushed = site.url("/search?q=query&at=");
    // One same-document settle; any slower settle path waits at least the
    // redirected quiet window.
    let bound_ms = worker_pool::navigation_settle::REDIRECTED_QUIET_MS;
    let mut failures = Vec::new();
    for run in 1..=10 {
        let loaded = live
            .call("navigate", json!({"url":site.url("/home")}))
            .await;
        assert_eq!(loaded["status"], "completed", "navigate: {loaded}");
        let typed = live
            .call(
                "type_text",
                json!({"target":{"role":"textbox","accessibleName":"Search"},
                       "value":"query\n","clearFirst":true}),
            )
            .await;
        let returned_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_millis();
        let mut navigations = Vec::new();
        objects_of_kind(&typed, "navigation", &mut navigations);
        let reported = navigations.iter().find_map(|item| {
            let at = item["url"].as_str()?.strip_prefix(pushed.as_str())?;
            (item["title"] == "Search results").then(|| at.parse::<u128>().ok())?
        });
        match reported {
            _ if typed["status"] != "completed" => {
                failures.push(format!("run {run}: status {}", typed["status"]));
            }
            None => failures.push(format!("run {run}: reported {navigations:?}")),
            Some(at) if returned_ms.saturating_sub(at) >= u128::from(bound_ms) => {
                failures.push(format!(
                    "run {run}: returned {} ms after Enter, bound {bound_ms} ms",
                    returned_ms.saturating_sub(at)
                ));
            }
            Some(_) => {}
        }
    }
    live.close().await;
    assert!(
        failures.is_empty(),
        "{} of 10 runs failed: {failures:?}",
        failures.len()
    );
}

/// Controls rendered a few seconds after load: every action and intent waits
/// for its target to appear instead of failing targetNotFound at once.
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
                "<a href='/more'>More results</a>";
              document.getElementById("late-button").addEventListener("click", () => {
                document.getElementById("status").textContent = "clicked";
              });
            }, 3000);
        </script>"#,
    );
    let site = FixtureSite::spawn(vec![
        ("/late", Route::Html(late)),
        (
            "/more",
            Route::Html(page("More", "<main><h1>More results</h1></main>")),
        ),
    ])
    .await;
    let live = Live::open(rig, &site.url("/more")).await;
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
                "purpose":"Show more results",
                "hints":{"role":"link","accessibleName":"More results"},
                "expectedState":{
                    "condition":{"kind":"url","matcher":{"kind":"contains","value":"/more"}},
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

/// Controls inside open shadow roots, one nested in another: the snapshot
/// lists them and its targets type, click and follow as on any other control.
pub async fn shadow_root_controls_act_from_snapshot_targets(rig: &Rig) {
    let home = page(
        "Home",
        r#"<main><outer-part></outer-part><p role="status" aria-label="idle" id="status"></p></main>
        <script>
          customElements.define("inner-part", class extends HTMLElement {
            constructor() {
              super();
              this.attachShadow({mode: "open"}).innerHTML =
                '<button data-id="deep">Deep button</button>';
            }
          });
          customElements.define("outer-part", class extends HTMLElement {
            constructor() {
              super();
              this.attachShadow({mode: "open"}).innerHTML =
                '<a href="/inside">Inside link</a><input aria-label="Inside field"><inner-part></inner-part>';
            }
          });
          document.addEventListener("click", (event) => {
            const reached = event.composedPath()[0];
            if (reached.dataset && reached.dataset.id) {
              document.getElementById("status").setAttribute("aria-label", "acted " + reached.dataset.id);
            }
          }, true);
        </script>"#,
    );
    let site = FixtureSite::spawn(vec![
        ("/home", Route::Html(home)),
        ("/inside", Route::Html(page("Inside", "<h1>Inside</h1>"))),
    ])
    .await;
    let live = Live::open(rig, &site.url("/home")).await;
    let snapshot = live.snapshot(json!({})).await;
    let mut targets = Vec::new();
    targets_under(&snapshot, &mut targets);
    let target = |role: &str, name: &str| {
        targets
            .iter()
            .find(|(found, target)| *found == role && target["accessibleName"] == name)
            .map(|(_, target)| (*target).clone())
            .unwrap_or_else(|| panic!("no {role} \"{name}\" target in the snapshot: {snapshot}"))
    };
    let field = target("textbox", "Inside field");
    let deep = target("button", "Deep button");
    let link = target("link", "Inside link");
    let typed = live
        .call("type_text", json!({"target":field,"value":"query"}))
        .await;
    assert_eq!(typed["status"], "completed", "type_text {field}: {typed}");
    let clicked = live.call("click", json!({"target":deep})).await;
    assert_eq!(clicked["status"], "completed", "click {deep}: {clicked}");
    let after = live.snapshot(json!({})).await;
    assert!(
        find_node(&after, "status", Some("acted deep")).is_some(),
        "the click did not reach the nested button: {after}"
    );
    let followed = live
        .call(
            "intent_follow",
            json!({"purpose":"Open the inside page","hints":link,
                   "expectedState":{"condition":{"kind":"url","matcher":{"kind":"contains","value":"/inside"}},
                                    "timeoutMs":10000}}),
        )
        .await;
    assert_eq!(
        followed["status"], "completed",
        "intent_follow {link}: {followed}"
    );
    live.close().await;
}

/// A link drawn under another link that covers its whole container: click
/// fails targetObscured and never follows the covering link.
pub async fn click_refuses_a_covered_target(rig: &Rig) {
    let home = page(
        "Home",
        r#"<main><div style="position:relative;width:480px;height:120px">
            <a href="/over" aria-label="Over" style="position:absolute;inset:0;z-index:2"></a>
            <a href="/under" style="position:relative;z-index:1">Under</a>
        </div></main>"#,
    );
    let site = FixtureSite::spawn(vec![
        ("/home", Route::Html(home)),
        ("/over", Route::Html(page("Over", "<h1>Over</h1>"))),
        ("/under", Route::Html(page("Under", "<h1>Under</h1>"))),
    ])
    .await;
    let live = Live::open(rig, &site.url("/home")).await;
    let clicked = live
        .call(
            "click",
            json!({"target":{"role":"link","accessibleName":"Under"}}),
        )
        .await;
    assert_eq!(
        clicked["error"]["code"], "targetObscured",
        "click on a covered link: {clicked}"
    );
    let (listed_url, _) = listed_page(&live).await;
    assert_eq!(
        listed_url,
        site.url("/home").as_str(),
        "the click followed another link"
    );
    live.close().await;
}

/// A link whose click opens a dialog instead of its destination: the follow
/// fails `obstructionSuspected` and its evidence names the dialog.
pub async fn intent_follow_reports_a_dialog_that_opened(rig: &Rig) {
    let home = page(
        "Home",
        r#"<main><a href="/next" id="next">Next</a></main>
        <div role="dialog" aria-modal="true" aria-label="Notice" hidden
             style="position:fixed;inset:0;background:#fff"><button>Close</button></div>
        <script>
        document.getElementById('next').addEventListener('click', (event) => {
            event.preventDefault();
            document.querySelector('[role="dialog"]').hidden = false;
        });
        </script>"#,
    );
    let site = FixtureSite::spawn(vec![
        ("/home", Route::Html(home)),
        ("/next", Route::Html(page("Next", "<h1>Next</h1>"))),
    ])
    .await;
    let live = Live::open(rig, &site.url("/home")).await;
    let followed = live
        .call(
            "intent_follow",
            json!({
                "purpose":"Open the next page",
                "hints":{"role":"link","accessibleName":"Next"},
                "expectedDestination":{
                    "condition":{"kind":"url","matcher":{"kind":"contains","value":"/next"}},
                    "timeoutMs":3000
                }
            }),
        )
        .await;
    assert_eq!(
        followed["error"]["code"], "obstructionSuspected",
        "follow that opened a dialog: {followed}"
    );
    assert!(
        followed.to_string().contains("Notice"),
        "the evidence does not name the dialog: {followed}"
    );
    live.close().await;
}

/// Closing a modal reveals a banner with its own Close button: dismiss
/// completes because the clicked control is gone, whatever else shares its name.
pub async fn dismiss_completes_when_a_same_named_control_appears(rig: &Rig) {
    let home = page(
        "Home",
        r#"<main><h1>Home</h1></main>
        <div role="region" aria-label="Banner" hidden>
            <button aria-label="Close" data-action="close-banner">x</button></div>
        <div role="dialog" aria-modal="true" aria-label="Notice"
             style="position:fixed;inset:0;background:#fff">
            <button aria-label="Close" data-action="close-dialog">x</button></div>
        <script>
        document.querySelector('[data-action="close-dialog"]').addEventListener('click', () => {
            document.querySelector('[role="dialog"]').remove();
            document.querySelector('[aria-label="Banner"]').hidden = false;
        });
        </script>"#,
    );
    let site = FixtureSite::spawn(vec![("/home", Route::Html(home))]).await;
    let live = Live::open(rig, &site.url("/home")).await;
    let dismissed = live
        .call(
            "intent_dismiss_obstruction",
            json!({"purpose":"close the notice","hints":{"role":"button","accessibleName":"Close"}}),
        )
        .await;
    assert_eq!(
        dismissed["status"], "completed",
        "dismiss beside a same-named control: {dismissed}"
    );
    let banner = live
        .snapshot(json!({"target":{"role":"region","accessibleName":"Banner"}}))
        .await;
    assert_eq!(
        banner["status"], "completed",
        "the banner did not appear after the close: {banner}"
    );
    live.close().await;
}

/// An expected `hidden` state holds once the control is removed, as well as
/// once it is hidden.
pub async fn hidden_state_holds_for_a_removed_control(rig: &Rig) {
    let body = r#"<main><button id="more">Show more</button>
        <button id="panel">Hide panel</button></main>
        <script>
          document.getElementById("more").addEventListener("click", (event) => event.target.remove());
          document.getElementById("panel").addEventListener("click", (event) => {
            event.target.style.display = "none";
          });
        </script>"#;
    let site = FixtureSite::spawn(vec![("/home", Route::Html(page("Home", body)))]).await;
    let live = Live::open(rig, &site.url("/home")).await;
    for name in ["Show more", "Hide panel"] {
        let followed = live
            .call(
                "intent_follow",
                json!({
                    "purpose":"activate the control",
                    "hints":{"role":"button","accessibleName":name},
                    "expectedState":{
                        "condition":{"kind":"element","target":{"role":"button","accessibleName":name},"state":"hidden"},
                        "timeoutMs":3000
                    }
                }),
            )
            .await;
        assert_eq!(
            followed["status"], "completed",
            "{name} did not reach the hidden state: {followed}"
        );
    }
    live.close().await;
}

/// Tabs, menu items, options and tree items get snapshot targets, and each
/// target clicks the element it names.
pub async fn snapshot_targets_cover_widget_items(rig: &Rig) {
    let body = r##"<main>
          <div role="tablist" aria-label="Views"><div role="tab" tabindex="0" data-id="tab">Replies</div></div>
          <div role="menu" aria-label="Order">
            <div role="menuitem" tabindex="-1" data-id="menuitem">Newest</div>
            <div role="menuitemcheckbox" aria-checked="false" tabindex="-1" data-id="menuitemcheckbox">Compact</div>
            <div role="menuitemradio" aria-checked="false" tabindex="-1" data-id="menuitemradio">Oldest</div></div>
          <div role="listbox" aria-label="Sizes"><div role="option" aria-selected="false" data-id="option">Large</div></div>
          <div role="tree" aria-label="Files"><div role="treeitem" data-id="treeitem">Docs</div></div>
          <p role="status" aria-label="idle" id="status"></p></main>
        <script>
          document.addEventListener("click", (event) => {
            const reached = event.target.closest("[data-id]");
            if (reached) document.getElementById("status").setAttribute("aria-label", "acted " + reached.dataset.id);
          }, true);
        </script>"##;
    let site = FixtureSite::spawn(vec![("/widgets", Route::Html(page("Widgets", body)))]).await;
    let live = Live::open(rig, &site.url("/widgets")).await;
    let snapshot = live.snapshot(json!({})).await;
    for (role, name, reached) in [
        ("tab", "Replies", "tab"),
        ("menuitem", "Newest", "menuitem"),
        ("menuitemcheckbox", "Compact", "menuitemcheckbox"),
        ("menuitemradio", "Oldest", "menuitemradio"),
        ("option", "Large", "option"),
        ("treeitem", "Docs", "treeitem"),
    ] {
        let target = find_node(&snapshot, role, Some(name))
            .and_then(|node| node.get("target"))
            .unwrap_or_else(|| panic!("no target for {role} {name:?}: {snapshot}"))
            .clone();
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

/// A form whose submit handler comes from a script fetched after the load
/// event (which a slow script holds back): navigate returns once that script
/// has run, so an immediate Enter hits the handler, not a native submit.
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
                   "value":"query\n","clearFirst":true}),
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
        names.contains(&"handled query"),
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

const ENTRY_COUNT: usize = 30;
/// Serialized `workflow_observe` result for the entries page before text
/// deduplication; the deduplicated result must be at most half of it.
const CHROMIUM_ENTRIES_UNDEDUPED_BYTES: usize = 34_456;
const FIREFOX_ENTRIES_UNDEDUPED_BYTES: usize = 34_867;

fn entry_text(index: usize) -> String {
    format!(
        "Entry {index:02}: a neutral sample paragraph standing in for a long post body, \
         written only so that this link text runs well past two hundred characters \
         and every repeated copy of it in an observation costs real bytes."
    )
}

/// A list whose items each nest a list item around a long link and a
/// button. The observation carries each text once, loses none, and every
/// target taken from it acts on its own element.
pub async fn observation_carries_each_text_once(rig: &Rig) {
    let mut items = String::new();
    for index in 0..ENTRY_COUNT {
        items.push_str(&format!(
            r##"<li><ul><li><a href="#entry-{index}" data-id="link-{index}">{}</a><button type="button" data-id="button-{index}">Save</button></li></ul></li>"##,
            entry_text(index)
        ));
    }
    let body = format!(
        r#"<main><h1>Recent entries</h1><ul>{items}</ul><ol id="log"></ol></main>
        <script>
          document.addEventListener("click", (event) => {{
            const reached = event.target.closest("[data-id]");
            if (!reached) return;
            event.preventDefault();
            const entry = document.createElement("li");
            entry.textContent = "acted " + reached.dataset.id;
            document.getElementById("log").append(entry);
          }}, true);
        </script>"#
    );
    let site = FixtureSite::spawn(vec![("/entries", Route::Html(page("Entry list", &body)))]).await;
    let live = Live::open(rig, &site.url("/entries")).await;
    let observed = live.observe(json!({"maxNodes":2048})).await;
    assert_eq!(
        observed["status"], "completed",
        "workflow_observe: {observed}"
    );
    let bytes = serde_json::to_string(&observed)
        .expect("observation serializes")
        .len();
    eprintln!(
        "entries observation, {}: {bytes} bytes",
        if rig.is_firefox() {
            "firefox"
        } else {
            "chromium"
        }
    );

    let mut texts = Vec::new();
    strings_under(&observed, "name", &mut texts);
    strings_under(&observed, "accessibleName", &mut texts);
    let page_texts = (0..ENTRY_COUNT)
        .map(entry_text)
        .chain(["Recent entries".to_owned(), "Save".to_owned()]);
    for text in page_texts {
        assert!(
            texts.iter().any(|seen| seen.contains(text.as_str())),
            "{text:?} is missing from the observation: {observed}"
        );
    }

    let mut targets = Vec::new();
    targets_under(&observed, &mut targets);
    let mut acted = Vec::new();
    for (role, target) in targets {
        let name = target["accessibleName"].as_str().unwrap_or_default();
        let id = match role {
            "link" => (0..ENTRY_COUNT)
                .find(|index| entry_text(*index) == name)
                .map(|index| format!("link-{index}")),
            "button" if name == "Save" => target["ordinal"]
                .as_u64()
                .map(|ordinal| format!("button-{ordinal}")),
            _ => None,
        };
        let id = id.unwrap_or_else(|| panic!("unexpected observation target {target}"));
        acted.push((target.clone(), id));
    }
    assert_eq!(
        acted.len(),
        2 * ENTRY_COUNT,
        "observation targets: {observed}"
    );
    for (target, _) in &acted {
        let clicked = live.call("click", json!({"target":target})).await;
        assert_eq!(clicked["status"], "completed", "click {target}: {clicked}");
    }
    let after = live.snapshot(json!({"maxNodes":2048})).await;
    let mut log = Vec::new();
    strings_under(&after, "name", &mut log);
    log.retain(|text| text.starts_with("acted "));
    let expected: Vec<String> = acted.iter().map(|(_, id)| format!("acted {id}")).collect();
    assert_eq!(log, expected, "clicks reached other elements: {after}");

    let undeduped = if rig.is_firefox() {
        FIREFOX_ENTRIES_UNDEDUPED_BYTES
    } else {
        CHROMIUM_ENTRIES_UNDEDUPED_BYTES
    };
    assert!(
        bytes * 2 <= undeduped,
        "observation is {bytes} bytes, more than half of {undeduped}: {observed}"
    );
    live.close().await;
}

const SIGN_IN_IDENTIFIER: &str = "reader@example.test";
const SIGN_IN_PASSWORD: &str = "fixture-pass-41";

/// A sign-in form whose identifier field offers passkeys
/// (`autocomplete="username webauthn"`) beside a password field, a divider
/// and a sign-in frame that may request credentials. Fields are named by
/// their labels and targetable, empty values read empty, and only the typed
/// password is redacted.
pub async fn sign_in_fields_show_their_labels(rig: &Rig) {
    let body = r#"<main><h1>Welcome back</h1>
        <form action="/signin" onsubmit="event.preventDefault()">
          <iframe title="Sign in" src="/signin-frame" allow="identity-credentials-get"></iframe>
          <div><span>or</span></div>
          <label for="identifier">Email or username</label>
          <input id="identifier" name="identifier" type="text" autocomplete="username webauthn">
          <label for="password">Password</label>
          <input id="password" name="password" type="password" autocomplete="current-password">
          <button type="submit">Next</button>
        </form></main>"#;
    let site = FixtureSite::spawn(vec![
        ("/signin", Route::Html(page("Sign in", body))),
        (
            "/signin-frame",
            Route::Html(page("Frame", "<button>Continue with a passkey</button>")),
        ),
    ])
    .await;
    let live = Live::open(rig, &site.url("/signin")).await;
    let snapshot = live.snapshot(json!({})).await;
    let observed = live.observe(json!({})).await;
    for (what, result) in [
        ("a11y_snapshot", &snapshot),
        ("workflow_observe", &observed),
    ] {
        assert_eq!(result["status"], "completed", "{what}: {result}");
        for label in ["Email or username", "Password"] {
            let field = find_node(result, "textbox", Some(label))
                .unwrap_or_else(|| panic!("{what} has no field named {label:?}: {result}"));
            assert_eq!(
                field["target"]["accessibleName"], label,
                "{what} gave the {label:?} field no target: {result}"
            );
            assert!(
                field["value"].as_str().unwrap_or_default().is_empty(),
                "{what} reported the empty {label:?} field as non-empty: {result}"
            );
        }
        // Chromium reports the frame's role as `Iframe`, the companion as `iframe`.
        assert!(
            ["iframe", "Iframe"]
                .iter()
                .any(|role| find_node(result, role, Some("Sign in")).is_some()),
            "{what} has no frame named \"Sign in\": {result}"
        );
        assert!(
            !result.to_string().contains("[redacted]"),
            "{what} redacted text on a form with nothing secret in it: {result}"
        );
    }
    let mut texts = Vec::new();
    strings_under(&snapshot, "name", &mut texts);
    strings_under(&observed, "name", &mut texts);
    assert!(
        texts.contains(&"or"),
        "the divider text is missing: {snapshot} {observed}"
    );

    for (label, value) in [
        ("Email or username", SIGN_IN_IDENTIFIER),
        ("Password", SIGN_IN_PASSWORD),
    ] {
        let target = find_node(&snapshot, "textbox", Some(label)).expect("field found above")
            ["target"]
            .clone();
        let typed = live
            .call(
                "type_text",
                json!({"target":target,"value":value,"clearFirst":true}),
            )
            .await;
        assert_eq!(typed["status"], "completed", "type_text {label:?}: {typed}");
    }
    let after = live.snapshot(json!({})).await;
    let identifier = find_node(&after, "textbox", Some("Email or username"))
        .unwrap_or_else(|| panic!("identifier field missing after typing: {after}"));
    assert_eq!(
        identifier["value"], SIGN_IN_IDENTIFIER,
        "the identifier field does not hold the typed value: {after}"
    );
    let password = find_node(&after, "textbox", Some("Password"))
        .unwrap_or_else(|| panic!("password field missing after typing: {after}"));
    assert_eq!(
        password["value"], "[redacted]",
        "the typed password is not redacted: {after}"
    );
    let observed_after = live.observe(json!({})).await;
    for (what, result) in [
        ("a11y_snapshot", &after),
        ("workflow_observe", &observed_after),
    ] {
        assert!(
            !result.to_string().contains(SIGN_IN_PASSWORD),
            "{what} exposed the typed password: {result}"
        );
    }
    live.close().await;
}

/// Sessions the runtime serves at once, its default capacity.
const CONCURRENT_SESSIONS: usize = 8;

/// Eight sessions run at once on one runtime: each opens its own page, types
/// into it, reads back only its own text, and lists only its own page.
pub async fn concurrent_sessions_each_keep_their_own_page(rig: &Rig) {
    let paths: Vec<String> = (0..CONCURRENT_SESSIONS)
        .map(|index| format!("/session/{index}"))
        .collect();
    let routes = paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let body = r#"<main><input placeholder="Note"></main>"#;
            (
                path.as_str(),
                Route::Html(page(&format!("Session {index}"), body)),
            )
        })
        .collect();
    let site = FixtureSite::spawn(routes).await;
    let urls: Vec<String> = paths.iter().map(|path| site.url(path)).collect();
    let sessions =
        futures_util::future::join_all(urls.iter().map(|url| Live::open(rig, url))).await;
    let checks = sessions
        .iter()
        .zip(&paths)
        .enumerate()
        .map(|(index, (live, path))| async move {
            let note = format!("note {index}");
            let typed = live
                .call(
                    "type_text",
                    json!({"target":{"role":"textbox","accessibleName":"Note"},
                           "value":note,"clearFirst":true}),
                )
                .await;
            assert_eq!(
                typed["status"], "completed",
                "session {index} type_text: {typed}"
            );
            let snapshot = live.snapshot(json!({})).await;
            assert_eq!(
                textbox_value(&snapshot, "Note"),
                Some(note.as_str()),
                "session {index} reads another session's text: {snapshot}"
            );
            let listed = live
                .rig
                .tool("page_list", json!({"sessionId":live.session_id}))
                .await;
            let mut urls = Vec::new();
            strings_under(&listed, "url", &mut urls);
            assert!(
                !urls.is_empty() && urls.iter().all(|url| url.ends_with(path.as_str())),
                "session {index} lists pages it does not own: {listed}"
            );
        });
    futures_util::future::join_all(checks).await;
    for live in sessions {
        live.close().await;
    }
}
