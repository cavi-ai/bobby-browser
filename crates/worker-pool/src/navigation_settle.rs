//! The in-page probe both engines use to decide a navigation has settled.

use std::time::Duration;

/// Longest a navigation waits for the document to stop changing.
pub const NAVIGATION_SETTLE_CAP: Duration = Duration::from_secs(5);
/// Time without a DOM mutation that counts as settled.
pub const NAVIGATION_QUIET_MS: u64 = 300;

/// A promise that resolves to a JSON string `{url, title}` once the document
/// has had no DOM mutation for [`NAVIGATION_QUIET_MS`], or after `cap_ms`.
pub fn navigation_settle_expression(cap_ms: u128) -> String {
    format!(
        "new Promise(resolve=>{{let timer;const read=()=>JSON.stringify({{url:location.href,title:document.title}});const finish=()=>{{observer.disconnect();clearTimeout(timer);clearTimeout(cap);resolve(read());}};const observer=new MutationObserver(()=>{{clearTimeout(timer);timer=setTimeout(finish,{NAVIGATION_QUIET_MS});}});observer.observe(document,{{subtree:true,childList:true,attributes:true,characterData:true}});timer=setTimeout(finish,{NAVIGATION_QUIET_MS});const cap=setTimeout(finish,{cap_ms});}})"
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
