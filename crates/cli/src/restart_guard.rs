//! Guards a restart of the shared runtime owner: what it would disconnect,
//! whether to go ahead, and a record of what was live.
use anyhow::{Context, Result};
use serde::Deserialize;
use std::{
    io::{BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
    time::Duration,
};

const MAX_SESSION_LINES: usize = 16;

pub(crate) const REFUSAL: &str = "refusing to restart: this disconnects every attached agent. Ask the operator to run `bobby runtime restart` in a terminal. Pass --disconnect-agents only when the operator has told you to.";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Impact {
    pub connections: usize,
    pub in_flight_commands: usize,
    pub sessions: Vec<SessionImpact>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionImpact {
    pub session_id: String,
    pub profile: String,
    pub last_used_at: String,
    pub pages: Vec<PageImpact>,
    pub recoverable_workflows: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PageImpact {
    pub url: Option<String>,
}

/// An impact report and the response text it was parsed from.
#[derive(Debug)]
pub(crate) struct Known {
    pub impact: Impact,
    pub raw: String,
}

impl Known {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        Some(Self {
            impact: serde_json::from_str(raw).ok()?,
            raw: raw.to_owned(),
        })
    }
}

#[derive(Debug)]
pub(crate) enum ImpactOutcome {
    /// Nothing is running.
    NoOwner,
    Known(Known),
    /// An owner is running but did not report: hung, older build, unreachable.
    Unknown,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Proceed,
    Prompt,
    Refuse,
}

pub(crate) fn decide(outcome: &ImpactOutcome, terminal: bool, disconnect_agents: bool) -> Decision {
    let idle = match outcome {
        ImpactOutcome::NoOwner => true,
        ImpactOutcome::Known(known) => {
            known.impact.connections == 0
                && known.impact.sessions.is_empty()
                && known.impact.in_flight_commands == 0
        }
        ImpactOutcome::Unknown => false,
    };
    if idle || disconnect_agents {
        Decision::Proceed
    } else if terminal {
        Decision::Prompt
    } else {
        Decision::Refuse
    }
}

/// Whether both stdin and stdout are a terminal, so a person can be asked.
pub(crate) fn terminal() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// The running owner a restart would replace.
pub(crate) struct Target {
    pub pid: Option<u32>,
    pub url: Option<String>,
}

pub(crate) fn render_summary(
    pid: Option<u32>,
    url: Option<&str>,
    outcome: &ImpactOutcome,
) -> String {
    let mut lines = vec![format!(
        "runtime owner pid {} {}",
        pid.map_or_else(|| "unknown".to_owned(), |pid| pid.to_string()),
        url.unwrap_or("-")
    )];
    match outcome {
        ImpactOutcome::Known(Known { impact, .. }) => {
            lines.push(format!(
                "  {} agent connection(s) attached, {} session(s), {} command(s) in flight",
                impact.connections,
                impact.sessions.len(),
                impact.in_flight_commands
            ));
            for session in impact.sessions.iter().take(MAX_SESSION_LINES) {
                let recoverable = if session.recoverable_workflows.is_empty() {
                    "no".to_owned()
                } else {
                    format!("yes ({} workflow(s))", session.recoverable_workflows.len())
                };
                lines.push(format!(
                    "  session {} profile {} last used {} pages {} {} recoverable: {}",
                    session.session_id,
                    session.profile,
                    session.last_used_at,
                    session.pages.len(),
                    session
                        .pages
                        .first()
                        .and_then(|page| page.url.as_deref())
                        .unwrap_or("-"),
                    recoverable
                ));
            }
            if impact.sessions.len() > MAX_SESSION_LINES {
                lines.push(format!(
                    "  +{} more",
                    impact.sessions.len() - MAX_SESSION_LINES
                ));
            }
        }
        _ => lines.push(
            "  impact unknown: the running runtime did not report what is attached".to_owned(),
        ),
    }
    lines.join("\n")
}

/// Ask whether to go ahead; only `y` or `yes` (any case) says yes.
pub(crate) fn confirm(input: &mut dyn BufRead, out: &mut dyn Write) -> std::io::Result<bool> {
    write!(out, "Restart and disconnect them? [y/N] ")?;
    out.flush()?;
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        return Ok(false);
    }
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// Ask the owner what a restart would hit. Anything but a decodable 200 is `Unknown`.
pub(crate) async fn read_impact(url: &str, secret: &str) -> ImpactOutcome {
    let Ok(origin) = url::Url::parse(url) else {
        return ImpactOutcome::Unknown;
    };
    let loopback = origin.scheme() == "http"
        && origin
            .host_str()
            .and_then(|host| host.parse::<std::net::IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback());
    if !loopback {
        return ImpactOutcome::Unknown;
    }
    let fetched = async {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .build()
            .ok()?;
        let response = client
            .get(format!("{url}/_bobby/runtime/impact"))
            .header("x-bobby-owner-stop", secret)
            .send()
            .await
            .ok()?;
        if response.status() != reqwest::StatusCode::OK {
            return None;
        }
        Known::parse(&response.text().await.ok()?)
    }
    .await;
    fetched.map_or(ImpactOutcome::Unknown, ImpactOutcome::Known)
}

#[derive(Clone, Copy)]
pub(crate) struct Verdict {
    pub terminal: bool,
    pub disconnect_agents: bool,
}

#[derive(Debug)]
pub(crate) enum Guard {
    Proceed,
    /// Nothing was stopped; the message says why.
    Refused(String),
}

fn write_snapshot(runtime_dir: &Path, pid: Option<u32>, raw: &str) -> Result<PathBuf> {
    let dir = runtime_dir.join("restart-snapshots");
    crate::runtime_scopes::private_dir(&dir)?;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
    let name = match pid {
        Some(pid) => format!("{stamp}-{pid}.json"),
        None => format!("{stamp}-unknown.json"),
    };
    let path = dir.join(name);
    let mut file = crate::runtime_scopes::private_options()
        .open(&path)
        .with_context(|| format!("cannot write restart snapshot {}", path.display()))?;
    file.write_all(raw.as_bytes())?;
    file.sync_all()?;
    Ok(path)
}

/// Decide whether the restart goes ahead. On `Proceed` the impact report has
/// been saved (when known); nothing has been stopped either way.
pub(crate) fn guard(
    target: &Target,
    outcome: &ImpactOutcome,
    verdict: Verdict,
    runtime_dir: &Path,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<Guard> {
    let summary = || render_summary(target.pid, target.url.as_deref(), outcome);
    match decide(outcome, verdict.terminal, verdict.disconnect_agents) {
        Decision::Refuse => {
            writeln!(out, "{}", summary())?;
            return Ok(Guard::Refused(REFUSAL.to_owned()));
        }
        Decision::Prompt => {
            writeln!(out, "{}", summary())?;
            if !confirm(input, out)? {
                return Ok(Guard::Refused("restart cancelled".to_owned()));
            }
        }
        Decision::Proceed => {}
    }
    match outcome {
        ImpactOutcome::Known(known) => {
            let path = write_snapshot(runtime_dir, target.pid, &known.raw)?;
            writeln!(out, "snapshot: {}", path.display())?;
        }
        ImpactOutcome::Unknown => writeln!(
            out,
            "snapshot unavailable: the running runtime did not report what is attached"
        )?,
        ImpactOutcome::NoOwner => {}
    }
    Ok(Guard::Proceed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn known(connections: usize, in_flight: usize, sessions: serde_json::Value) -> ImpactOutcome {
        let raw = json!({
            "ownerId": "00000000-0000-0000-0000-000000000001",
            "takenAt": "2026-10-05T10:00:00Z",
            "connections": connections,
            "inFlightCommands": in_flight,
            "sessions": sessions,
        })
        .to_string();
        ImpactOutcome::Known(Known::parse(&raw).unwrap())
    }

    fn session(n: usize, url: Option<&str>, workflows: usize) -> serde_json::Value {
        let pages = match url {
            Some(url) => json!([{"pageId": "p1", "url": url}, {"pageId": "p2"}]),
            None => json!([{"pageId": "p1"}]),
        };
        json!({
            "sessionId": format!("s{n}"),
            "profile": "default",
            "createdAt": "2026-10-05T09:00:00Z",
            "lastUsedAt": "2026-10-05T09:59:00Z",
            "pages": pages,
            "recoverableWorkflows": (0..workflows).map(|i| format!("w{i}")).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn decision_table_covers_every_combination() {
        let idle = || known(0, 0, json!([]));
        let attached = || known(1, 0, json!([]));
        let sessions = || known(0, 0, json!([session(1, None, 0)]));
        let busy = || known(0, 2, json!([]));
        for terminal in [false, true] {
            for flag in [false, true] {
                assert_eq!(
                    decide(&ImpactOutcome::NoOwner, terminal, flag),
                    Decision::Proceed
                );
                assert_eq!(decide(&idle(), terminal, flag), Decision::Proceed);
                for outcome in [attached(), sessions(), busy(), ImpactOutcome::Unknown] {
                    let expected = if flag {
                        Decision::Proceed
                    } else if terminal {
                        Decision::Prompt
                    } else {
                        Decision::Refuse
                    };
                    assert_eq!(
                        decide(&outcome, terminal, flag),
                        expected,
                        "terminal={terminal} flag={flag} {outcome:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn summary_shows_counts_sessions_urls_and_recoverability() {
        let outcome = known(
            2,
            1,
            json!([
                session(1, Some("https://example.com/a"), 2),
                session(2, None, 0)
            ]),
        );
        let text = render_summary(Some(41), Some("http://127.0.0.1:9"), &outcome);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "runtime owner pid 41 http://127.0.0.1:9");
        assert_eq!(
            lines[1],
            "  2 agent connection(s) attached, 2 session(s), 1 command(s) in flight"
        );
        assert_eq!(
            lines[2],
            "  session s1 profile default last used 2026-10-05T09:59:00Z pages 2 https://example.com/a recoverable: yes (2 workflow(s))"
        );
        assert_eq!(
            lines[3],
            "  session s2 profile default last used 2026-10-05T09:59:00Z pages 1 - recoverable: no"
        );
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn summary_truncates_at_sixteen_sessions() {
        let many: Vec<_> = (0..20).map(|n| session(n, None, 0)).collect();
        let text = render_summary(Some(1), Some("http://x"), &known(1, 0, json!(many)));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2 + 16 + 1);
        assert_eq!(lines[18], "  +4 more");
        let exact: Vec<_> = (0..16).map(|n| session(n, None, 0)).collect();
        let text = render_summary(Some(1), Some("http://x"), &known(1, 0, json!(exact)));
        assert!(!text.contains("more"));
    }

    #[test]
    fn summary_for_unknown_impact_says_so() {
        let text = render_summary(Some(7), Some("http://127.0.0.1:1"), &ImpactOutcome::Unknown);
        assert_eq!(
            text,
            "runtime owner pid 7 http://127.0.0.1:1\n  impact unknown: the running runtime did not report what is attached"
        );
    }

    #[test]
    fn prompt_proceeds_only_on_yes() {
        for (input, expected) in [
            ("y\n", true),
            ("Y\n", true),
            ("yes\n", true),
            ("YES\n", true),
            ("\n", false),
            ("n\n", false),
            ("maybe\n", false),
            ("", false),
        ] {
            let mut out = Vec::new();
            let answer = confirm(&mut input.as_bytes(), &mut out).unwrap();
            assert_eq!(answer, expected, "{input:?}");
            assert_eq!(
                String::from_utf8(out).unwrap(),
                "Restart and disconnect them? [y/N] "
            );
        }
    }

    #[test]
    fn guard_refuses_without_a_terminal_and_writes_no_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = known(1, 0, json!([]));
        let mut out = Vec::new();
        let result = guard(
            &Target {
                pid: Some(5),
                url: Some("http://x".into()),
            },
            &outcome,
            Verdict {
                terminal: false,
                disconnect_agents: false,
            },
            dir.path(),
            &mut "".as_bytes(),
            &mut out,
        )
        .unwrap();
        match result {
            Guard::Refused(message) => assert_eq!(message, REFUSAL),
            Guard::Proceed => panic!("must refuse"),
        }
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("1 agent connection"));
        assert!(!dir.path().join("restart-snapshots").exists());
    }

    #[test]
    fn guard_cancels_on_a_no_answer_and_proceeds_with_a_snapshot_on_yes() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = known(1, 0, json!([]));
        let target = Target {
            pid: Some(5),
            url: Some("http://x".into()),
        };
        let verdict = Verdict {
            terminal: true,
            disconnect_agents: false,
        };
        let mut out = Vec::new();
        match guard(
            &target,
            &outcome,
            verdict,
            dir.path(),
            &mut "n\n".as_bytes(),
            &mut out,
        )
        .unwrap()
        {
            Guard::Refused(message) => assert_eq!(message, "restart cancelled"),
            Guard::Proceed => panic!("must cancel"),
        }
        assert!(!dir.path().join("restart-snapshots").exists());
        let mut out = Vec::new();
        assert!(matches!(
            guard(
                &target,
                &outcome,
                verdict,
                dir.path(),
                &mut "y\n".as_bytes(),
                &mut out
            )
            .unwrap(),
            Guard::Proceed
        ));
        let out = String::from_utf8(out).unwrap();
        let line = out.split("snapshot: ").nth(1).expect(&out);
        let path = std::path::PathBuf::from(line.trim());
        assert!(path.starts_with(dir.path().join("restart-snapshots")));
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("-5.json"));
        let parsed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(parsed["connections"], 1);
    }

    #[test]
    fn guard_reports_an_unavailable_snapshot_for_unknown_impact() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let result = guard(
            &Target {
                pid: Some(5),
                url: None,
            },
            &ImpactOutcome::Unknown,
            Verdict {
                terminal: false,
                disconnect_agents: true,
            },
            dir.path(),
            &mut "".as_bytes(),
            &mut out,
        )
        .unwrap();
        assert!(matches!(result, Guard::Proceed));
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("snapshot unavailable: the running runtime did not report what is attached"));
        assert!(!dir.path().join("restart-snapshots").exists());
    }

    async fn serve_once(response: &'static str, hang: bool) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 2048];
            let _ = socket.read(&mut buffer).await;
            if hang {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            } else {
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        url
    }

    #[tokio::test]
    async fn impact_client_parses_200_and_maps_every_other_answer_to_unknown() {
        let body = r#"{"ownerId":"00000000-0000-0000-0000-000000000001","takenAt":"2026-10-05T10:00:00Z","connections":3,"inFlightCommands":0,"sessions":[]}"#;
        let ok: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        );
        match read_impact(&serve_once(ok, false).await, "secret").await {
            ImpactOutcome::Known(known) => {
                assert_eq!(known.impact.connections, 3);
                assert_eq!(known.raw, body);
            }
            other => panic!("{other:?}"),
        }
        for status in [
            "404 Not Found",
            "503 Service Unavailable",
            "401 Unauthorized",
        ] {
            let response: &'static str = Box::leak(
                format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .into_boxed_str(),
            );
            assert!(matches!(
                read_impact(&serve_once(response, false).await, "secret").await,
                ImpactOutcome::Unknown
            ));
        }
        let garbage: &'static str =
            "HTTP/1.1 200 OK\r\ncontent-length: 3\r\nconnection: close\r\n\r\nnot";
        assert!(matches!(
            read_impact(&serve_once(garbage, false).await, "secret").await,
            ImpactOutcome::Unknown
        ));
        let started = std::time::Instant::now();
        assert!(matches!(
            read_impact(&serve_once("", true).await, "secret").await,
            ImpactOutcome::Unknown
        ));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }
}
