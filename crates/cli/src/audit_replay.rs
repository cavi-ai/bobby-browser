//! A static, self-contained HTML replay of one verified audit bundle: every
//! command in journal order with its phases, outcome, evidence, and the
//! screenshots the bundle carries. No script; the page renders from the file
//! alone, so it can be attached to a review or shipped in the docs.

use std::fmt::Write as _;

use anyhow::Result;
use base64::Engine as _;
use serde_json::Value;

use crate::audit_bundle::VerifiedBundle;

struct Command {
    id: String,
    envelope: Option<Value>,
    phases: Vec<(String, String)>,
    outcome: Option<Value>,
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}

/// "click #submit", "fill · Email address", "navigate https://…".
fn label(envelope: Option<&Value>) -> String {
    let Some(command) = envelope.map(|envelope| &envelope["command"]) else {
        return "command (envelope not journaled)".to_owned();
    };
    let inner = &command["input"];
    let kind = text(&inner["kind"]);
    let input = &inner["input"];
    let detail = ["purpose", "url", "selector", "description"]
        .iter()
        .map(|key| text(&input[*key]))
        .find(|value| !value.is_empty())
        .unwrap_or("");
    let family = text(&command["kind"]);
    let head = if family == "intent" {
        format!("intent {kind}")
    } else {
        kind.to_owned()
    };
    if detail.is_empty() {
        head
    } else {
        format!("{head} · {detail}")
    }
}

fn status_class(status: &str) -> &'static str {
    match status {
        "completed" => "ok",
        "needsReconciliation" | "retryableFailure" | "restarted" => "warn",
        "" => "open",
        _ => "bad",
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

fn commands(journal: &[u8]) -> Vec<Command> {
    let mut commands: Vec<Command> = Vec::new();
    for line in journal.split(|byte| *byte == b'\n') {
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let id = text(&record["commandId"]).to_owned();
        let index = match commands.iter().position(|command| command.id == id) {
            Some(index) => index,
            None => {
                commands.push(Command {
                    id,
                    envelope: None,
                    phases: Vec::new(),
                    outcome: None,
                });
                commands.len() - 1
            }
        };
        let command = &mut commands[index];
        if command.envelope.is_none() && record["envelope"].is_object() {
            command.envelope = Some(record["envelope"].clone());
        }
        command.phases.push((
            text(&record["phase"]).to_owned(),
            text(&record["recordedAt"]).to_owned(),
        ));
        if record["outcome"].is_object() {
            command.outcome = Some(record["outcome"].clone());
        }
    }
    commands
}

fn screenshot(bundle: &VerifiedBundle, evidence: &Value) -> Option<String> {
    let artifact = text(&evidence["artifactId"]);
    if evidence["kind"] != "screenshot" || artifact.is_empty() {
        return None;
    }
    let bytes = bundle
        .files
        .get(&format!("artifacts/{artifact}/{artifact}.png"))?;
    Some(base64::engine::general_purpose::STANDARD.encode(bytes))
}

const STYLE: &str = "\
body{margin:0;background:#f6f7f5;color:#1b1f1c;font:14px/1.5 -apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif}\
main{max-width:980px;margin:0 auto;padding:24px 16px 64px}\
h1{font-size:22px;margin:0 0 4px}h2{font-size:16px;margin:0}\
.meta{color:#4a514c;font-size:12.5px;margin:0 0 20px}.meta code{word-break:break-all}\
code,pre{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:12px}\
pre{background:#eef0ec;border-radius:6px;padding:10px;overflow-x:auto;white-space:pre-wrap;word-break:break-word}\
section{background:#fff;border:1px solid #d8dcd6;border-radius:8px;padding:14px 16px;margin:0 0 12px}\
.head{display:flex;gap:10px;align-items:baseline;flex-wrap:wrap}\
.n{color:#7b837d;font-family:ui-monospace,Menlo,monospace}\
.badge{border-radius:999px;padding:1px 9px;font-size:11.5px;font-family:ui-monospace,Menlo,monospace}\
.ok{background:#dcefe3;color:#2b6e46}.warn{background:#f6e3cf;color:#8c4f0a}.bad{background:#f5dcd9;color:#a3322a}.open{background:#eef0ec;color:#4a514c}\
ol.phases{list-style:none;padding:0;margin:8px 0;display:flex;flex-wrap:wrap;gap:4px 12px;color:#4a514c;font-size:12.5px}\
.error{color:#a3322a}img{max-width:100%;border:1px solid #d8dcd6;border-radius:4px;margin:8px 0}\
details{margin:6px 0}summary{cursor:pointer;color:#4a514c}\
@media (prefers-color-scheme:dark){body{background:#15181a;color:#e8eae6}section{background:#1d2124;border-color:#343a3e}\
pre{background:#252a2e}.meta,.phases,summary{color:#b9bfb8}}";

/// The replay page for `bundle`. Deterministic: the same bundle renders the
/// same bytes.
pub fn render(bundle: &VerifiedBundle) -> Result<String> {
    let summary = &bundle.summary;
    let mut html = String::new();
    write!(
        html,
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>Workflow replay {id}</title><style>{STYLE}</style></head><body><main>\
         <h1>Workflow replay</h1><p class=\"meta\">workflow <code>{id}</code> · sessions {sessions} · \
         exported {created} by bobby {version} · {files} files, digests and signature verified · \
         signer <code>{signer}</code> ({pinned})</p>",
        id = escape(&summary.workflow_id),
        sessions = escape(&bundle.session_ids.join(", ")),
        created = escape(&bundle.created_at.to_rfc3339()),
        version = escape(&bundle.bobby_version),
        files = summary.files,
        signer = escape(&summary.public_key),
        pinned = if summary.pinned {
            "pinned"
        } else {
            "not pinned"
        },
    )?;
    if !summary.missing_artifacts.is_empty() {
        write!(
            html,
            "<p class=\"meta\">artifacts no longer on disk at export: {}</p>",
            escape(&summary.missing_artifacts.join(", "))
        )?;
    }
    let journal = bundle
        .files
        .get("journal.jsonl")
        .map(Vec::as_slice)
        .unwrap_or_default();
    for (index, command) in commands(journal).iter().enumerate() {
        let status = command
            .outcome
            .as_ref()
            .map(|outcome| text(&outcome["status"]).to_owned())
            .unwrap_or_default();
        write!(
            html,
            "<section><div class=\"head\"><span class=\"n\">{n}</span><h2>{label}</h2>\
             <span class=\"badge {class}\">{status}</span></div>\
             <div class=\"meta\">command <code>{id}</code></div><ol class=\"phases\">",
            n = index + 1,
            label = escape(&label(command.envelope.as_ref())),
            class = status_class(&status),
            status = escape(if status.is_empty() {
                "no outcome"
            } else {
                &status
            }),
            id = escape(&command.id),
        )?;
        for (phase, at) in &command.phases {
            write!(
                html,
                "<li>{} <code>{}</code></li>",
                escape(phase),
                escape(at)
            )?;
        }
        html.push_str("</ol>");
        let outcome = command.outcome.as_ref();
        if let Some(message) = outcome
            .map(|outcome| text(&outcome["error"]["message"]))
            .filter(|message| !message.is_empty())
        {
            write!(html, "<p class=\"error\">{}</p>", escape(message))?;
        }
        let evidence = outcome
            .and_then(|outcome| outcome["evidence"].as_array())
            .map(Vec::as_slice)
            .unwrap_or_default();
        for item in evidence {
            if let Some(png) = screenshot(bundle, item) {
                write!(
                    html,
                    "<img alt=\"screenshot {}\" src=\"data:image/png;base64,{png}\">",
                    escape(text(&item["artifactId"]))
                )?;
            }
        }
        if !evidence.is_empty() {
            let kinds = evidence
                .iter()
                .map(|item| text(&item["kind"]))
                .collect::<Vec<_>>()
                .join(", ");
            write!(
                html,
                "<details><summary>evidence: {}</summary><pre>{}</pre></details>",
                escape(&kinds),
                escape(&pretty(&Value::Array(evidence.to_vec())))
            )?;
        }
        if let Some(envelope) = &command.envelope {
            write!(
                html,
                "<details><summary>command</summary><pre>{}</pre></details>",
                escape(&pretty(&envelope["command"]))
            )?;
        }
        html.push_str("</section>");
    }
    if let Some(checkpoint) = bundle
        .files
        .get("checkpoint.json")
        .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
    {
        write!(
            html,
            "<section><div class=\"head\"><h2>Checkpoint</h2></div><details open><summary>checkpoint.json</summary><pre>{}</pre></details></section>",
            escape(&pretty(&checkpoint))
        )?;
    }
    html.push_str("</main></body></html>\n");
    Ok(html)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit_bundle::{export, load_or_create_key, open_verified, BundleSources};
    use chrono::Utc;
    use types::{
        AttemptId, CommandEnvelope, CommandError, CommandId, CommandOutcome, CommandPhase,
        ErrorCode, ErrorLayer, Evidence, NavigateCommand, PageId, PrimitiveCommand, RuntimeCommand,
        SessionId, WaitUntil, WorkflowId,
    };
    use workflow_journal::{CommandJournal, JournalRecord, JsonlJournal};

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nnot a real image";

    async fn bundle() -> (tempfile::TempDir, VerifiedBundle) {
        let root = tempfile::tempdir().unwrap();
        let sources = BundleSources {
            journal: root.path().join("commands.jsonl"),
            checkpoints_dir: root.path().join("checkpoints"),
            artifacts_dir: root.path().join("artifacts"),
        };
        let workflow = WorkflowId::new();
        let session = SessionId::new();
        let journal = JsonlJournal::open(&sources.journal).await.unwrap();
        let navigate = CommandId::new();
        let failed = CommandId::new();
        for (command, url) in [
            (&navigate, "https://shop.test/"),
            (&failed, "https://shop.test/<pay>"),
        ] {
            journal
                .append(JournalRecord {
                    sequence: 0,
                    recorded_at: Utc::now(),
                    command_id: command.clone(),
                    phase: CommandPhase::Executing,
                    envelope: Some(CommandEnvelope {
                        schema_version: CommandEnvelope::SCHEMA_VERSION,
                        command_id: command.clone(),
                        workflow_id: workflow.clone(),
                        attempt_id: AttemptId::new(),
                        session_id: session.clone(),
                        page_id: Some(PageId::new()),
                        deadline: Utc::now() + chrono::Duration::seconds(30),
                        command: RuntimeCommand::Primitive(PrimitiveCommand::Navigate(
                            NavigateCommand {
                                url: url.into(),
                                wait_until: WaitUntil::DomContentLoaded,
                                timeout_ms: 10_000,
                            },
                        )),
                    }),
                    outcome: None,
                    prepared_result: None,
                })
                .await
                .unwrap();
        }
        let completed = CommandOutcome::Completed {
            command_id: navigate.clone(),
            evidence: vec![Evidence::Screenshot {
                artifact_id: "shot-1".into(),
                media_type: "image/png".into(),
                width: 1,
                height: 1,
                bytes: PNG.len() as u64,
                sha256: String::new(),
            }],
        };
        let rejected = CommandOutcome::Failed {
            command_id: failed.clone(),
            error: CommandError {
                code: ErrorCode::Internal,
                message: "<script>alert(1)</script> rejected".into(),
                layer: ErrorLayer::Page,
                retryable: false,
            },
            evidence: Vec::new(),
        };
        for (command, outcome) in [(&navigate, completed), (&failed, rejected)] {
            journal
                .append(JournalRecord {
                    sequence: 0,
                    recorded_at: Utc::now(),
                    command_id: command.clone(),
                    phase: CommandPhase::Completed,
                    envelope: None,
                    outcome: Some(outcome),
                    prepared_result: None,
                })
                .await
                .unwrap();
        }
        let artifact = artifact_store::artifact_dir(&sources.artifacts_dir, &session, "shot-1");
        std::fs::create_dir_all(&artifact).unwrap();
        std::fs::write(artifact.join("shot-1.png"), PNG).unwrap();
        let key = load_or_create_key(&root.path().join("key.pk8")).unwrap();
        let out = root.path().join("bundle.tar");
        export(&sources, &workflow, &key, &out).unwrap();
        let verified = open_verified(&out, None).unwrap();
        (root, verified)
    }

    #[tokio::test]
    async fn replay_renders_each_command_with_phases_outcome_and_screenshot() {
        let (_root, bundle) = bundle().await;
        let html = render(&bundle).unwrap();
        assert!(
            html.contains("<h2>navigate · https://shop.test/</h2>"),
            "{html}"
        );
        assert!(html.contains("<span class=\"badge ok\">completed</span>"));
        assert!(html.contains("<span class=\"badge bad\">failed</span>"));
        assert!(html.contains("<li>executing <code>"));
        let png = base64::engine::general_purpose::STANDARD.encode(PNG);
        assert!(html.contains(&format!("src=\"data:image/png;base64,{png}\"")));
        assert!(html.contains("digests and signature verified"));
        assert_eq!(
            render(&bundle).unwrap(),
            html,
            "the same bundle renders the same bytes"
        );
    }

    #[tokio::test]
    async fn replay_escapes_page_and_journal_text() {
        let (_root, bundle) = bundle().await;
        let html = render(&bundle).unwrap();
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt; rejected"));
        assert!(html.contains("navigate · https://shop.test/&lt;pay&gt;"));
    }
}
