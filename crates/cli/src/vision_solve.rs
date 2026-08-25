//! `bobby vision solve`: submit a `SolveChallenge` intent against a running
//! runtime — create (or reuse) a session, optionally navigate, then let the
//! engine's vision loop work the challenge.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{Duration as ChronoDuration, Utc};
use types::{
    AttemptId, CommandEnvelope, CommandId, CreateSessionRequest, ExecutionPolicy, IntentCommand,
    NavigateCommand, OpenPageRequest, PageId, PrimitiveCommand, RuntimeCommand, SessionId,
    SolveChallengeHints, SolveChallengeIntent, WaitUntil, WorkflowId,
};
use uuid::Uuid;

use crate::v1_client;

/// Extra slack on top of the solve budget for navigation, session setup,
/// and the model's own latency on the terminal reassessment.
const DEADLINE_SLACK: ChronoDuration = ChronoDuration::seconds(90);

pub struct VisionSolveOptions {
    pub purpose: String,
    pub url: Option<String>,
    pub session: Option<String>,
    pub page: Option<String>,
    pub node: String,
    pub timeout_ms: u64,
    /// ZigZagZig mode: every session capability on — humanized input timing,
    /// fingerprint spoofing, and JS evaluation alongside vision assist.
    pub zigzagzig: bool,
    pub base_url: String,
    pub bearer: String,
}

pub fn solve(options: VisionSolveOptions) -> Result<()> {
    let (session_id, page_id) = match (&options.session, &options.page) {
        (Some(session), Some(page)) => (parse_session_id(session)?, parse_page_id(page)?),
        (None, None) => open_session_and_page(&options)?,
        _ => bail!("--session and --page must be given together"),
    };

    if let Some(url) = &options.url {
        let envelope = envelope(
            &session_id,
            &page_id,
            RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
                url: url.clone(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 30_000,
            })),
            options.timeout_ms,
        );
        let outcome = submit(&options, &envelope)?;
        if outcome["status"] != "completed" {
            bail!("navigate to {url} failed: {}", outcome_message(&outcome));
        }
    }

    let envelope = envelope(
        &session_id,
        &page_id,
        RuntimeCommand::Intent(IntentCommand::SolveChallenge(SolveChallengeIntent {
            purpose: options.purpose.clone(),
            hints: SolveChallengeHints {
                region: None,
                timeout_ms: options.timeout_ms,
            },
        })),
        options.timeout_ms,
    );
    let outcome = submit(&options, &envelope)?;
    if outcome["status"] == "completed" {
        let evidence = outcome["evidence"].as_array().map_or(0, Vec::len);
        println!("ok: challenge solved ({evidence} evidence items)");
        for verification in intent_verifications(&outcome) {
            println!("  {verification}");
        }
        return Ok(());
    }
    bail!("solve failed: {}", outcome_message(&outcome));
}

fn open_session_and_page(options: &VisionSolveOptions) -> Result<(SessionId, PageId)> {
    let policy = ExecutionPolicy {
        vision_assist: true,
        vision_node: Some(options.node.clone()),
        // ZigZagZig is the everything-on mode: humanized timing, fingerprint
        // spoofing, and JS evaluation alongside the vision assist the solve
        // loop needs. Without the flag the session is vision-only.
        javascript_evaluation: options.zigzagzig,
        fingerprint: options.zigzagzig,
        humanize: options.zigzagzig,
    };
    let session = post(
        options,
        "/v1/sessions",
        &CreateSessionRequest {
            profile: "vision-solve".into(),
            proxy: None,
            execution_policy: policy,
        },
    )?;
    let session_id = parse_session_id(
        session["id"]
            .as_str()
            .context("session response has no id")?,
    )?;
    let page = post(
        options,
        "/v1/pages",
        &OpenPageRequest {
            session_id: session_id.clone(),
        },
    )?;
    let page_id = parse_page_id(page["id"].as_str().context("page response has no id")?)?;
    println!("session {} page {}", session_id.0, page_id.0);
    Ok((session_id, page_id))
}

fn envelope(
    session_id: &SessionId,
    page_id: &PageId,
    command: RuntimeCommand,
    timeout_ms: u64,
) -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: session_id.clone(),
        page_id: Some(page_id.clone()),
        deadline: Utc::now() + ChronoDuration::milliseconds(timeout_ms as i64) + DEADLINE_SLACK,
        command,
    }
}

fn submit(options: &VisionSolveOptions, envelope: &CommandEnvelope) -> Result<serde_json::Value> {
    post(options, "/v1/commands", envelope)
}

fn post<T: serde::Serialize>(
    options: &VisionSolveOptions,
    path: &str,
    body: &T,
) -> Result<serde_json::Value> {
    let response = v1_client::v1_request_with_limits(
        v1_client::V1Request {
            method: reqwest::Method::POST,
            url: v1_client::v1_url(&options.base_url, path)?,
            bearer: options.bearer.clone(),
            body: Some(serde_json::to_value(body)?),
            idempotency_key: None,
        },
        Duration::from_millis(options.timeout_ms) + Duration::from_secs(90),
        ChronoDuration::milliseconds(options.timeout_ms as i64) + DEADLINE_SLACK,
    )?;
    if !response.status.is_success() {
        bail!("POST {path} -> {}: {}", response.status, response.body);
    }
    v1_client::parse_json_body(response.status, &response.body)
}

fn parse_session_id(raw: &str) -> Result<SessionId> {
    Uuid::parse_str(raw)
        .map(SessionId)
        .with_context(|| format!("invalid session id: {raw}"))
}

fn parse_page_id(raw: &str) -> Result<PageId> {
    Uuid::parse_str(raw)
        .map(PageId)
        .with_context(|| format!("invalid page id: {raw}"))
}

fn outcome_message(outcome: &serde_json::Value) -> String {
    outcome["error"]["message"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| outcome.to_string())
}

fn intent_verifications(outcome: &serde_json::Value) -> Vec<String> {
    outcome["evidence"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["kind"] == "intentExecution")
        .filter_map(|item| {
            let record = &item["record"];
            Some(format!(
                "{}: {}",
                record["intentKind"].as_str()?,
                record["verification"].as_str()?
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_message_prefers_the_structured_error() {
        let outcome = serde_json::json!({
            "status": "failed",
            "error": { "code": "visionAssistFailed", "message": "below floor" }
        });
        assert_eq!(outcome_message(&outcome), "below floor");
    }

    #[test]
    fn intent_verifications_reads_only_intent_records() {
        let outcome = serde_json::json!({
            "evidence": [
                { "kind": "screenshot", "artifactId": "a" },
                { "kind": "intentExecution", "record": {
                    "intentKind": "solveChallenge",
                    "verification": "challengeSolved attempts=3"
                }}
            ]
        });
        assert_eq!(
            intent_verifications(&outcome),
            vec!["solveChallenge: challengeSolved attempts=3".to_string()]
        );
    }

    #[test]
    fn id_parsing_rejects_non_uuid_input() {
        assert!(parse_session_id("not-a-uuid").is_err());
        let uuid = Uuid::new_v4();
        assert_eq!(parse_page_id(&uuid.to_string()).unwrap().0, uuid);
    }
}
