//! The in-page probe both engines use to decide a navigation has settled.

use std::time::{Duration, Instant};

/// Longest a navigation waits for the document to stop changing.
pub const NAVIGATION_SETTLE_CAP: Duration = Duration::from_secs(5);
/// Time without a content change that counts as settled.
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
/// Most changed attribute names a capped probe reports.
const CHURN_TOP_ATTRIBUTES: usize = 5;
/// Longest attribute name a capped probe reports; longer names are dropped.
const CHURN_ATTRIBUTE_NAME_CHARS: usize = 40;
/// Distinct attribute names a probe counts.
const CHURN_TRACKED_ATTRIBUTE_NAMES: usize = 64;

/// A promise that resolves to a JSON string `{url, title, quiet, begin}` once
/// the document has loaded (`readyState` is `complete`, so every script it
/// parsed has run) and then had no content change for the quiet window
/// (`quiet: true`), or after `cap_ms` (`quiet: false`). A content change is
/// any DOM mutation except a node moved within the document and a node's
/// repeated `class`/`style` changes or text edits (animations, counters,
/// clocks); a node's first one in a probe still counts. `begin` is the URL
/// and title the probe started on. The window is [`REDIRECTED_QUIET_MS`] when
/// the document was reached through a server redirect (`performance`
/// navigation timing) or sits on a URL other than `requested_url`,
/// [`SAME_DOCUMENT_QUIET_MS`] when its URL (fragment aside) differs from the
/// one it loaded at, and [`NAVIGATION_QUIET_MS`] otherwise. Scripts and
/// fetch/XHR the page loads after its load are the caller's to wait for: each
/// engine sees them in its network tracker and runs the probe again. A capped
/// result adds `churn`: the counted changes by kind, whether they were inside
/// or outside `main`, and the most changed attribute names with counts.
pub fn navigation_settle_expression(cap_ms: u128, requested_url: &str) -> String {
    let requested = serde_json::to_string(requested_url).unwrap_or_else(|_| "\"\"".into());
    format!(
        "new Promise(resolve=>{{let timer;let done=false;\
const page=()=>({{url:location.href,title:document.title}});const begin=page();\
const tally={{added:0,removed:0,attributes:0,characterData:0,insideMain:0,outsideMain:0}};const names=new Map();\
const churn=()=>({{...tally,topAttributes:[...names].sort((a,b)=>b[1]-a[1]).slice(0,{CHURN_TOP_ATTRIBUTES})}});\
const finish=quiet=>{{if(done)return;done=true;observer.disconnect();document.removeEventListener('readystatechange',restart);clearTimeout(timer);clearTimeout(cap);resolve(JSON.stringify(quiet?{{...page(),quiet,begin}}:{{...page(),quiet,begin,churn:churn()}}));}};\
const entry=performance.getEntriesByType('navigation')[0];\
let moved=!!entry&&entry.redirectCount>0;\
try{{moved=moved||new URL({requested}).href!==location.href;}}catch(e){{}}\
const bare=url=>String(url).split('#')[0];\
const inPlace=!!entry&&bare(entry.name)!==bare(location.href);\
const quietMs=moved?{REDIRECTED_QUIET_MS}:inPlace?{SAME_DOCUMENT_QUIET_MS}:{NAVIGATION_QUIET_MS};\
const restart=()=>{{clearTimeout(timer);if(document.readyState==='complete')timer=setTimeout(()=>finish(true),quietMs);}};\
const edited=new WeakSet();\
const restyle=r=>r.type==='characterData'||r.attributeName==='class'||r.attributeName==='style';\
const note=r=>{{try{{const node=r.target.nodeType===1?r.target:r.target.parentElement;\
if(node&&node.closest('main,[role=main]'))tally.insideMain+=1;else tally.outsideMain+=1;\
if(r.type==='attributes'){{tally.attributes+=1;const name=r.attributeName;\
if(name.length<={CHURN_ATTRIBUTE_NAME_CHARS}&&(names.has(name)||names.size<{CHURN_TRACKED_ATTRIBUTE_NAMES}))names.set(name,(names.get(name)||0)+1);}}\
else if(r.type==='characterData')tally.characterData+=1;}}catch(e){{}}return true;}};\
const counted=(r,relocated)=>{{if(r.type==='childList'){{\
const added=[...r.addedNodes].filter(n=>!relocated(n)).length,removed=[...r.removedNodes].filter(n=>!relocated(n)).length;\
if(added+removed===0)return false;tally.added+=added;tally.removed+=removed;return note(r);}}\
if(restyle(r)&&(edited.has(r.target)||!edited.add(r.target)))return false;return note(r);}};\
const changes=records=>{{const added=new Set(),removed=new Set();\
records.forEach(r=>{{r.addedNodes.forEach(n=>added.add(n));r.removedNodes.forEach(n=>removed.add(n));}});\
const relocated=n=>added.has(n)&&removed.has(n);\
return records.filter(r=>counted(r,relocated)).length>0;}};\
const observer=new MutationObserver(records=>{{if(changes(records))restart();}});\
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
    /// What kept a capped probe from going quiet; `None` for a quiet one.
    pub churn: Option<SettleChurn>,
}

/// The changes that restarted a capped probe's quiet window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettleChurn {
    /// Nodes inserted, moves within the document aside.
    pub added: u64,
    /// Nodes removed, moves within the document aside.
    pub removed: u64,
    /// Attribute changes.
    pub attributes: u64,
    /// Text edits.
    pub character_data: u64,
    /// Whether any change was inside `main`.
    pub inside_main: bool,
    /// Whether any change was outside `main`.
    pub outside_main: bool,
    /// The five most changed attribute names of at most 40 characters, with
    /// their counts.
    pub top_attributes: Vec<(String, u64)>,
}

impl SettleChurn {
    /// `name=count` pairs separated by spaces; an attribute name holds neither.
    fn top_attributes_field(&self) -> String {
        self.top_attributes
            .iter()
            .map(|(name, count)| format!("{name}={count}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
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
        churn: value.get("churn").and_then(parse_churn),
    })
}

/// Decodes a capped probe's `churn`, keeping at most [`CHURN_TOP_ATTRIBUTES`]
/// attribute names that are short and contain no whitespace, control or `=`.
fn parse_churn(value: &serde_json::Value) -> Option<SettleChurn> {
    let count = |key: &str| value.get(key)?.as_u64();
    let top_attributes = value
        .get("topAttributes")?
        .as_array()?
        .iter()
        .filter_map(|pair| {
            let name = pair.get(0)?.as_str()?;
            let count = pair.get(1)?.as_u64()?;
            let traceable = !name.is_empty()
                && name.chars().count() <= CHURN_ATTRIBUTE_NAME_CHARS
                && !name
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control() || c == '=');
            traceable.then(|| (name.to_owned(), count))
        })
        .take(CHURN_TOP_ATTRIBUTES)
        .collect();
    Some(SettleChurn {
        added: count("added")?,
        removed: count("removed")?,
        attributes: count("attributes")?,
        character_data: count("characterData")?,
        inside_main: count("insideMain")? > 0,
        outside_main: count("outsideMain")? > 0,
        top_attributes,
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
/// unknown) and whether a load `landed` while the probe ran. Unknown loads
/// never count as none. An unknown read or a landed load costs one more probe,
/// once per settle: `extra_probe_spent` records it.
pub fn after_probe(
    pending: Option<usize>,
    landed: bool,
    extra_probe_spent: &mut bool,
) -> AfterProbe {
    match pending {
        Some(0) if !landed => AfterProbe::Settled,
        Some(0) | None => {
            if std::mem::replace(extra_probe_spent, true) {
                AfterProbe::Settled
            } else {
                AfterProbe::ProbeAgain
            }
        }
        Some(_) => AfterProbe::AwaitLoads,
    }
}

/// Logs one line per settle. URL and title stay out of the log; only whether
/// each changed between the first probe's start and the settled read. A settle
/// that ends at the cap also logs the last capped probe's `churn`.
pub fn trace_settle(
    engine: &'static str,
    exit: SettleExit,
    started: Instant,
    pending_page_loads: Option<usize>,
    begin: Option<&(String, String)>,
    settled: Option<&(String, String)>,
    churn: Option<&SettleChurn>,
) {
    let (url_changed, title_changed) = match (begin, settled) {
        (Some(begin), Some(settled)) => (begin.0 != settled.0, begin.1 != settled.1),
        _ => (false, false),
    };
    let pending_page_loads =
        pending_page_loads.map_or_else(|| "unknown".to_owned(), |count| count.to_string());
    let churn = churn.filter(|_| exit == SettleExit::Cap);
    let top_attributes = churn.map(SettleChurn::top_attributes_field);
    tracing::info!(
        engine,
        exit = exit.as_str(),
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        %pending_page_loads,
        url_changed,
        title_changed,
        nodes_added = churn.map(|churn| churn.added),
        nodes_removed = churn.map(|churn| churn.removed),
        attribute_changes = churn.map(|churn| churn.attributes),
        text_changes = churn.map(|churn| churn.character_data),
        inside_main = churn.map(|churn| churn.inside_main),
        outside_main = churn.map(|churn| churn.outside_main),
        top_attributes = top_attributes.as_deref(),
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

    fn read_with_churn(churn: serde_json::Value) -> ProbeRead {
        let encoded = serde_json::json!({
            "url": "u", "title": "t", "quiet": false,
            "begin": {"url": "u", "title": "t"}, "churn": churn,
        });
        parse_settled(&encoded.to_string()).expect("probe result parses")
    }

    #[test]
    fn a_capped_probe_reports_its_churn_and_a_quiet_one_none() {
        let read = read_with_churn(serde_json::json!({
            "added": 3, "removed": 1, "attributes": 40, "characterData": 2,
            "insideMain": 46, "outsideMain": 0,
            "topAttributes": [["data-progress", 38], ["aria-busy", 2]],
        }));
        let churn = read.churn.expect("capped probe carries churn");
        assert_eq!(
            churn,
            SettleChurn {
                added: 3,
                removed: 1,
                attributes: 40,
                character_data: 2,
                inside_main: true,
                outside_main: false,
                top_attributes: vec![("data-progress".into(), 38), ("aria-busy".into(), 2)],
            }
        );
        assert_eq!(churn.top_attributes_field(), "data-progress=38 aria-busy=2");
        let quiet = parse_settled(
            r#"{"url":"u","title":"t","quiet":true,"begin":{"url":"u","title":"t"}}"#,
        )
        .expect("probe result parses");
        assert_eq!(quiet.churn, None);
        assert_eq!(
            read_with_churn(serde_json::json!({"added": "3"})).churn,
            None,
            "a malformed churn keeps the read and drops the churn"
        );
    }

    #[test]
    fn churn_keeps_at_most_five_short_attribute_names() {
        let forty = "e".repeat(40);
        let names = [
            "d".repeat(41),
            "a b".into(),
            "x=y".into(),
            "tab\t".into(),
            String::new(),
            forty.clone(),
            "n1".into(),
            "n2".into(),
            "n3".into(),
            "n4".into(),
            "n5".into(),
        ];
        let pairs: Vec<_> = names
            .iter()
            .map(|name| serde_json::json!([name, 7]))
            .collect();
        let churn = read_with_churn(serde_json::json!({
            "added": 0, "removed": 0, "attributes": 77, "characterData": 0,
            "insideMain": 0, "outsideMain": 77, "topAttributes": pairs,
        }))
        .churn
        .expect("capped probe carries churn");
        let kept: Vec<&str> = churn
            .top_attributes
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(kept, [forty.as_str(), "n1", "n2", "n3", "n4"]);
        assert!(churn.outside_main && !churn.inside_main);
    }

    #[test]
    fn unknown_loads_cost_one_more_probe_and_never_count_as_none() {
        let mut spent = false;
        assert_eq!(
            after_probe(Some(2), false, &mut spent),
            AfterProbe::AwaitLoads
        );
        assert_eq!(after_probe(None, false, &mut spent), AfterProbe::ProbeAgain);
        assert_eq!(
            after_probe(Some(1), false, &mut spent),
            AfterProbe::AwaitLoads
        );
        assert_eq!(after_probe(None, false, &mut spent), AfterProbe::Settled);
        assert_eq!(after_probe(Some(0), false, &mut false), AfterProbe::Settled);
    }

    #[test]
    fn a_landed_load_and_an_unknown_read_share_one_extra_probe() {
        let mut spent = false;
        assert_eq!(
            after_probe(Some(0), true, &mut spent),
            AfterProbe::ProbeAgain
        );
        assert_eq!(after_probe(Some(0), true, &mut spent), AfterProbe::Settled);
        assert_eq!(after_probe(None, false, &mut spent), AfterProbe::Settled);
        assert_eq!(
            after_probe(Some(1), true, &mut spent),
            AfterProbe::AwaitLoads
        );
    }
}
