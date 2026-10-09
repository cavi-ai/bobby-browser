//! The in-page probe both engines use to decide a navigation has settled.

use std::time::{Duration, Instant};

/// Longest a navigation waits for the document to stop changing.
pub const NAVIGATION_SETTLE_CAP: Duration = Duration::from_secs(5);
/// Time without a DOM mutation that counts as settled.
pub const NAVIGATION_QUIET_MS: u64 = 300;
/// Quiet time required when the navigation was redirected. A script that
/// bounces a sign-in page back to the app (`location.replace`) mutates no DOM,
/// so the shorter window cannot tell a settled sign-in page from a pending
/// bounce. Only redirected navigations pay for it.
pub const REDIRECTED_QUIET_MS: u64 = 1_800;
/// Quiet time required once the document has changed its URL in place
/// (`pushState`): a single-page app can start its data request, or set its
/// title, up to this long after its last DOM change.
pub const SAME_DOCUMENT_QUIET_MS: u64 = 1_000;
/// How often a settle re-checks the page's in-flight script and fetch/XHR loads.
pub const LOAD_POLL: Duration = Duration::from_millis(50);

/// A promise that resolves to a JSON string `{url, title, quiet, begin}` once
/// the document has loaded (`readyState` is `complete`, so every script it
/// parsed has run) and then had no DOM mutation for the quiet window
/// (`quiet: true`), or after `cap_ms` (`quiet: false`). `begin` is the URL
/// and title the probe started on. The window is [`REDIRECTED_QUIET_MS`] when
/// the document was reached through a server redirect (`performance`
/// navigation timing) or sits on a URL other than `requested_url`,
/// [`SAME_DOCUMENT_QUIET_MS`] when its URL (fragment aside) differs from the
/// one it loaded at, and [`NAVIGATION_QUIET_MS`] otherwise. Scripts and
/// fetch/XHR the page loads after its load are the caller's to wait for: each
/// engine sees them in its network tracker and runs the probe again.
pub fn navigation_settle_expression(cap_ms: u128, requested_url: &str) -> String {
    let requested = serde_json::to_string(requested_url).unwrap_or_else(|_| "\"\"".into());
    format!(
        "new Promise(resolve=>{{let timer;let done=false;\
const page=()=>({{url:location.href,title:document.title}});const begin=page();\
const finish=quiet=>{{if(done)return;done=true;observer.disconnect();document.removeEventListener('readystatechange',restart);clearTimeout(timer);clearTimeout(cap);resolve(JSON.stringify({{...page(),quiet,begin}}));}};\
const entry=performance.getEntriesByType('navigation')[0];\
let moved=!!entry&&entry.redirectCount>0;\
try{{moved=moved||new URL({requested}).href!==location.href;}}catch(e){{}}\
const bare=url=>String(url).split('#')[0];\
const inPlace=!!entry&&bare(entry.name)!==bare(location.href);\
const quietMs=moved?{REDIRECTED_QUIET_MS}:inPlace?{SAME_DOCUMENT_QUIET_MS}:{NAVIGATION_QUIET_MS};\
const restart=()=>{{clearTimeout(timer);if(document.readyState==='complete')timer=setTimeout(()=>finish(true),quietMs);}};\
const observer=new MutationObserver(restart);\
observer.observe(document,{{subtree:true,childList:true,attributes:true,characterData:true}});\
document.addEventListener('readystatechange',restart);\
restart();const cap=setTimeout(()=>finish(false),{cap_ms});}})"
    )
}

/// One probe result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeRead {
    /// The URL and title when the probe resolved.
    pub page: (String, String),
    /// `false` when the probe's cap fired before the document went quiet.
    pub quiet: bool,
    /// The URL and title the probe started on.
    pub begin: (String, String),
}

/// Decodes the probe's result.
pub fn parse_settled(encoded: &str) -> Option<ProbeRead> {
    let value: serde_json::Value = serde_json::from_str(encoded).ok()?;
    let page = |value: &serde_json::Value| {
        Some((
            value.get("url")?.as_str()?.to_owned(),
            value.get("title")?.as_str()?.to_owned(),
        ))
    };
    Some(ProbeRead {
        page: page(&value)?,
        quiet: value.get("quiet")?.as_bool()?,
        begin: page(value.get("begin")?)?,
    })
}

/// How a settle ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleExit {
    /// The document went quiet with no script or fetch/XHR load in flight.
    Quiet,
    /// The budget ran out first.
    Cap,
    /// The page could not be read.
    Unreadable,
}

impl SettleExit {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quiet => "quiet",
            Self::Cap => "cap",
            Self::Unreadable => "unreadable",
        }
    }
}

/// What a settle does once a probe resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterProbe {
    /// No script or fetch/XHR load is pending: the settle ends.
    Settled,
    /// Loads are in flight: wait for them, then probe again.
    AwaitLoads,
    /// The loads are unknown: probe once more before ending.
    ProbeAgain,
}

/// Decides the step after a probe from the page's pending loads (`None` when
/// unknown). Unknown loads never count as none: the first unknown read costs
/// one more probe. `unknown_reads` counts those reads across the settle.
pub fn after_probe(pending: Option<usize>, unknown_reads: &mut u32) -> AfterProbe {
    match pending {
        Some(0) => AfterProbe::Settled,
        Some(_) => AfterProbe::AwaitLoads,
        None => {
            *unknown_reads += 1;
            if *unknown_reads > 1 {
                AfterProbe::Settled
            } else {
                AfterProbe::ProbeAgain
            }
        }
    }
}

/// Logs one line per settle. URL and title stay out of the log; only whether
/// each changed between the first probe's start and the settled read.
pub fn trace_settle(
    engine: &'static str,
    exit: SettleExit,
    started: Instant,
    pending_page_loads: Option<usize>,
    begin: Option<&(String, String)>,
    settled: Option<&(String, String)>,
) {
    let (url_changed, title_changed) = match (begin, settled) {
        (Some(begin), Some(settled)) => (begin.0 != settled.0, begin.1 != settled.1),
        _ => (false, false),
    };
    let pending_page_loads =
        pending_page_loads.map_or_else(|| "unknown".to_owned(), |count| count.to_string());
    tracing::info!(
        engine,
        exit = exit.as_str(),
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        %pending_page_loads,
        url_changed,
        title_changed,
        "navigation settle"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_settled_reads_the_page_quiet_flag_and_start() {
        let read = parse_settled(
            r#"{"url":"https://a.test/b","title":"B","quiet":false,"begin":{"url":"https://a.test/a","title":"A"}}"#,
        )
        .expect("probe result parses");
        assert_eq!(read.page, ("https://a.test/b".into(), "B".into()));
        assert!(!read.quiet);
        assert_eq!(read.begin, ("https://a.test/a".into(), "A".into()));
        assert_eq!(parse_settled(r#"{"url":"u","title":"t"}"#), None);
    }

    #[test]
    fn unknown_loads_cost_one_more_probe_and_never_count_as_none() {
        let mut unknown_reads = 0;
        assert_eq!(
            after_probe(Some(2), &mut unknown_reads),
            AfterProbe::AwaitLoads
        );
        assert_eq!(
            after_probe(None, &mut unknown_reads),
            AfterProbe::ProbeAgain
        );
        assert_eq!(
            after_probe(Some(1), &mut unknown_reads),
            AfterProbe::AwaitLoads
        );
        assert_eq!(after_probe(None, &mut unknown_reads), AfterProbe::Settled);
        assert_eq!(after_probe(Some(0), &mut 0), AfterProbe::Settled);
    }
}
