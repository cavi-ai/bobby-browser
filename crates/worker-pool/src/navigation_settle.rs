//! The in-page probe both engines use to decide a navigation has settled.

use std::time::Duration;

/// Longest a navigation waits for the document to stop changing.
pub const NAVIGATION_SETTLE_CAP: Duration = Duration::from_secs(5);
/// Time without a DOM mutation that counts as settled.
pub const NAVIGATION_QUIET_MS: u64 = 300;
/// Quiet time required when the navigation was redirected. A script that
/// bounces a sign-in page back to the app (`location.replace`) mutates no DOM,
/// so the shorter window cannot tell a settled sign-in page from a pending
/// bounce. Only redirected navigations pay for it.
pub const REDIRECTED_QUIET_MS: u64 = 1_800;
/// How often a settle re-checks the page's in-flight script and fetch/XHR loads.
pub const LOAD_POLL: Duration = Duration::from_millis(50);

/// A promise that resolves to a JSON string `{url, title}` once the document
/// has loaded (`readyState` is `complete`, so every script it parsed has run)
/// and then had no DOM mutation for the quiet window, or after `cap_ms`. The
/// window is [`REDIRECTED_QUIET_MS`] when the document was reached through a
/// server redirect (`performance` navigation timing) or sits on a URL other
/// than `requested_url`, and [`NAVIGATION_QUIET_MS`] otherwise. Scripts and
/// fetch/XHR the page loads after its load are the caller's to wait for: each
/// engine sees them in its network tracker and runs the probe again.
pub fn navigation_settle_expression(cap_ms: u128, requested_url: &str) -> String {
    let requested = serde_json::to_string(requested_url).unwrap_or_else(|_| "\"\"".into());
    format!(
        "new Promise(resolve=>{{let timer;let done=false;\
const read=()=>JSON.stringify({{url:location.href,title:document.title}});\
const finish=()=>{{if(done)return;done=true;observer.disconnect();document.removeEventListener('readystatechange',restart);clearTimeout(timer);clearTimeout(cap);resolve(read());}};\
const entry=performance.getEntriesByType('navigation')[0];\
let moved=!!entry&&entry.redirectCount>0;\
try{{moved=moved||new URL({requested}).href!==location.href;}}catch(e){{}}\
const quiet=moved?{REDIRECTED_QUIET_MS}:{NAVIGATION_QUIET_MS};\
const restart=()=>{{clearTimeout(timer);if(document.readyState==='complete')timer=setTimeout(finish,quiet);}};\
const observer=new MutationObserver(restart);\
observer.observe(document,{{subtree:true,childList:true,attributes:true,characterData:true}});\
document.addEventListener('readystatechange',restart);\
restart();const cap=setTimeout(finish,{cap_ms});}})"
    )
}

/// Decodes the probe's result into `(url, title)`.
pub fn parse_settled(encoded: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(encoded).ok()?;
    Some((
        value.get("url")?.as_str()?.to_owned(),
        value.get("title")?.as_str()?.to_owned(),
    ))
}
