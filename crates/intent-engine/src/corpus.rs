//! Vision-escalation corpus collection.
//!
//! When `[vision].corpus_dir` is configured, every escalation through
//! `escalate_with_vision` appends one JSONL record to
//! `<corpus_dir>/vision-corpus.jsonl`: a masked screenshot, sanitized
//! structural context, a candidate-only proposal, the terminal outcome stage,
//! and — for verified clicks — the target index resolved via
//! `element_at_point`.
//!
//! Records are schema-agnostic (raw action kinds + `target_index`), matching
//! the gauntlet corpus contract; `build_completion` converts at training time.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::vision::VisionAction;

/// One escalation, serialized as one JSONL line in the training format.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CorpusRecord {
    pub image_b64: String,
    pub purpose: String,
    pub intent_kind: String,
    pub stuck: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_url: Option<String>,
    pub context_candidates: Vec<CorpusCandidate>,
    /// Index into `context_candidates` of the element the executed action
    /// actually hit. `None` when the action was not a verified click, the
    /// worker cannot resolve points, or the resolution matched no candidate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_index: Option<usize>,
    /// What `element_at_point` saw, for offline review when matching failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_element: Option<ResolvedElement>,
    pub model_response: CorpusModelResponse,
    pub success: bool,
    pub journey: String,
    pub step: String,
    /// Terminal engine stage: `visionFallback`, `visionActFailed:<verify>`,
    /// `visionRejectionFloor`, `visionProposeFailed`, `visionScreenshotFailed`.
    pub outcome_stage: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CorpusCandidate {
    pub role: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedElement {
    pub role: String,
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct CorpusModelResponse {
    pub confidence: f32,
    pub action: serde_json::Value,
}

/// Read-only health of a vision corpus JSONL file. Never creates or rewrites the path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CorpusHealth {
    pub exists: bool,
    pub bytes: u64,
    pub records: usize,
    pub torn_tail: bool,
    pub corrupt_line: Option<usize>,
}

/// Appends corpus records to `<dir>/vision-corpus.jsonl`.
#[derive(Debug, Clone)]
pub struct VisionCorpus {
    path: PathBuf,
}

impl VisionCorpus {
    pub fn new(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        set_private_dir_permissions(dir)?;
        Ok(Self {
            path: dir.join("vision-corpus.jsonl"),
        })
    }

    pub fn record(&self, record: &CorpusRecord) {
        let line = match sanitized_record(record).and_then(|record| serde_json::to_string(&record))
        {
            Ok(line) => line,
            Err(error) => {
                tracing::warn!(%error, "vision.corpus_serialize_failed");
                return;
            }
        };
        if let Err(error) = append_line(&self.path, &line) {
            tracing::warn!(%error, path = %self.path.display(), "vision.corpus_write_failed");
        }
    }

    /// Probe corpus health without creating directories or rewriting the file.
    pub fn inspect(path: impl AsRef<Path>) -> std::io::Result<CorpusHealth> {
        let path = path.as_ref();
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CorpusHealth::default());
            }
            Err(error) => return Err(error),
        };
        let torn_tail = !bytes.is_empty() && !bytes.ends_with(b"\n");
        let complete_len = if torn_tail {
            bytes
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |at| at + 1)
        } else {
            bytes.len()
        };
        let mut records = 0;
        let mut corrupt_line = None;
        for (index, line) in bytes[..complete_len]
            .split(|byte| *byte == b'\n')
            .enumerate()
        {
            if line.is_empty() {
                continue;
            }
            match serde_json::from_slice::<serde_json::Value>(line) {
                Ok(serde_json::Value::Object(_)) => records += 1,
                _ => {
                    corrupt_line = Some(index + 1);
                    break;
                }
            }
        }
        Ok(CorpusHealth {
            exists: true,
            bytes: bytes.len() as u64,
            records,
            torn_tail,
            corrupt_line,
        })
    }
}

fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    set_private_file_permissions(path)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn sanitized_record(record: &CorpusRecord) -> Result<Value, serde_json::Error> {
    let mut value = serde_json::to_value(record)?;
    let Some(object) = value.as_object_mut() else {
        return Ok(value);
    };
    object.insert("privacyVersion".into(), Value::from(1));
    object.remove("errorMessage");
    sanitize_optional_url(object, "contextUrl");
    sanitize_text_field(object, "purpose");
    sanitize_text_field(object, "stuck");
    sanitize_text_field(object, "journey");
    sanitize_text_field(object, "step");
    sanitize_text_field(object, "outcomeStage");
    sanitize_named_rows(object.get_mut("contextCandidates"));
    sanitize_named_row(object.get_mut("resolvedElement"));

    let target_index = object.get("targetIndex").and_then(Value::as_u64);
    if let Some(response) = object
        .get_mut("modelResponse")
        .and_then(Value::as_object_mut)
    {
        let action = response.remove("action").unwrap_or(Value::Null);
        response.insert(
            "action".into(),
            sanitize_corpus_action(&action, target_index),
        );
    }
    Ok(value)
}

fn sanitize_optional_url(object: &mut Map<String, Value>, field: &str) {
    let replacement = object
        .get(field)
        .and_then(Value::as_str)
        .and_then(sanitize_corpus_url)
        .map(Value::String);
    match replacement {
        Some(value) => {
            object.insert(field.to_owned(), value);
        }
        None => {
            object.remove(field);
        }
    }
}

fn sanitize_text_field(object: &mut Map<String, Value>, field: &str) {
    if let Some(value) = object.get_mut(field) {
        if let Some(text) = value.as_str() {
            *value = Value::String(sanitize_corpus_label(text));
        }
    }
}

fn sanitize_named_rows(value: Option<&mut Value>) {
    if let Some(rows) = value.and_then(Value::as_array_mut) {
        for row in rows {
            sanitize_named_row(Some(row));
        }
    }
}

fn sanitize_named_row(value: Option<&mut Value>) {
    if let Some(row) = value.and_then(Value::as_object_mut) {
        sanitize_text_field(row, "role");
        sanitize_text_field(row, "name");
    }
}

pub fn sanitize_corpus_action(action: &Value, target_index: Option<u64>) -> Value {
    let kind = action
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("abstain");
    let index = target_index.or_else(|| action.get("index").and_then(Value::as_u64));
    match (kind, index) {
        ("typeText" | "typeIntoCandidate" | "type_into_candidate", Some(index)) => {
            serde_json::json!({"kind": "typeIntoCandidate", "index": index})
        }
        ("extractValue" | "extractFromCandidate" | "extract_from_candidate", Some(index)) => {
            serde_json::json!({"kind": "extractFromCandidate", "index": index})
        }
        ("click" | "clickCandidate" | "click_candidate", Some(index)) => {
            serde_json::json!({"kind": "clickCandidate", "index": index})
        }
        ("challengeSolved", _) => serde_json::json!({"kind": "challengeSolved"}),
        ("noChallengeDetected", _) => serde_json::json!({"kind": "noChallengeDetected"}),
        ("challengeDetected", _) => serde_json::json!({"kind": "challengeDetected"}),
        _ => serde_json::json!({"kind": "abstain"}),
    }
}

/// Retains stable site and route identity while removing all URL-carried secrets.
pub fn sanitize_corpus_url(raw: &str) -> Option<String> {
    let mut url = url::Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    let segments = url
        .path_segments()
        .map(|segments| {
            let mut previous_sensitive = false;
            segments
                .map(|segment| {
                    let redact = previous_sensitive || dynamic_path_segment(segment);
                    previous_sensitive = sensitive_path_key(segment);
                    if redact {
                        ":id".to_owned()
                    } else {
                        segment.to_owned()
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if let Ok(mut path) = url.path_segments_mut() {
        path.clear();
        path.extend(segments);
    }
    let mut output = url.to_string();
    if output.ends_with('/') && url.path() != "/" {
        output.pop();
    }
    Some(output)
}

fn sensitive_path_key(segment: &str) -> bool {
    matches!(
        segment.to_ascii_lowercase().as_str(),
        "auth"
            | "code"
            | "credential"
            | "invite"
            | "key"
            | "magic"
            | "password"
            | "reset"
            | "secret"
            | "session"
            | "token"
    )
}

fn dynamic_path_segment(segment: &str) -> bool {
    let compact = segment
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric())
        .count();
    let uuid_shape = segment.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| segment.as_bytes().get(index) == Some(&b'-'));
    uuid_shape
        || (segment.len() >= 24
            && compact * 10 >= segment.len() * 8
            && segment.bytes().any(|byte| byte.is_ascii_digit())
            && segment.bytes().any(|byte| byte.is_ascii_alphabetic()))
}

pub fn sanitize_corpus_label(raw: &str) -> String {
    let text = raw
        .trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(512)
        .collect::<String>();
    let lower = text.to_ascii_lowercase();
    let credential_prefix = ["bearer ", "basic ", "sk-", "ghp_", "github_pat_", "akia"]
        .into_iter()
        .any(|marker| lower.contains(marker));
    if credential_prefix {
        return "[redacted]".into();
    }
    text.split_whitespace()
        .map(|token| {
            let inspected = token.trim_matches(|ch: char| {
                !ch.is_ascii_alphanumeric() && !matches!(ch, '@' | '.' | '_' | '-')
            });
            let email_value = inspected.contains('@')
                && inspected
                    .rsplit_once('@')
                    .is_some_and(|(_, domain)| domain.contains('.'));
            let high_entropy = inspected.len() >= 24
                && inspected
                    .bytes()
                    .filter(|byte| byte.is_ascii_alphanumeric())
                    .count()
                    * 10
                    >= inspected.len() * 8
                && inspected.bytes().any(|byte| byte.is_ascii_digit())
                && inspected.bytes().any(|byte| byte.is_ascii_alphabetic());
            if email_value || high_entropy {
                "[redacted]"
            } else {
                token
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Match a resolved (role, name) onto the candidates the model saw. Exact
/// name match, then case-insensitive; duplicates resolve to the first, and
/// the mismatch is visible via `resolved_element` in the record.
pub fn match_resolved(
    candidates: &[CorpusCandidate],
    resolved: &(String, String),
) -> Option<usize> {
    let (role, name) = resolved;
    if name.is_empty() {
        return None;
    }
    let role_matches =
        |candidate: &&CorpusCandidate| role.is_empty() || candidate.role.eq_ignore_ascii_case(role);
    candidates
        .iter()
        .position(|c| c.name.as_str() == name.as_str() && role_matches(&c))
        .or_else(|| {
            let lowered = name.to_lowercase();
            candidates
                .iter()
                .position(|c| c.name.to_lowercase() == lowered && role_matches(&c))
        })
        .or_else(|| {
            candidates
                .iter()
                .position(|c| c.name.as_str() == name.as_str())
        })
}

/// Serialize a vision action without runtime-owned text or extracted values.
pub fn raw_action(action: &VisionAction, target_index: Option<usize>) -> serde_json::Value {
    let index = target_index.and_then(|index| u64::try_from(index).ok());
    match action {
        VisionAction::Click { .. } => index.map_or_else(
            || serde_json::json!({"kind": "abstain"}),
            |index| serde_json::json!({"kind": "clickCandidate", "index": index}),
        ),
        VisionAction::TypeText { .. } => index.map_or_else(
            || serde_json::json!({"kind": "abstain"}),
            |index| serde_json::json!({"kind": "typeIntoCandidate", "index": index}),
        ),
        VisionAction::ExtractValue { .. } => index.map_or_else(
            || serde_json::json!({"kind": "abstain"}),
            |index| serde_json::json!({"kind": "extractFromCandidate", "index": index}),
        ),
        VisionAction::ClickCandidate { index } => {
            serde_json::json!({"kind": "clickCandidate", "index": index})
        }
        VisionAction::TypeIntoCandidate { index } => {
            serde_json::json!({"kind": "typeIntoCandidate", "index": index})
        }
        VisionAction::ExtractFromCandidate { index } => {
            serde_json::json!({"kind": "extractFromCandidate", "index": index})
        }
        VisionAction::ChallengeSolved => {
            serde_json::json!({"kind": "challengeSolved"})
        }
        VisionAction::ChallengeDetected {
            challenge_type,
            region,
            blocking,
        } => {
            serde_json::json!({
                "kind": "challengeDetected",
                "challengeType": serde_json::to_value(challenge_type).unwrap_or_default(),
                "region": region,
                "blocking": blocking,
            })
        }
        VisionAction::NoChallengeDetected => {
            serde_json::json!({"kind": "noChallengeDetected"})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VisionAction;

    fn candidate(role: &str, name: &str) -> CorpusCandidate {
        CorpusCandidate {
            role: role.into(),
            name: name.into(),
        }
    }

    #[test]
    fn exact_name_match_with_role() {
        let candidates = vec![candidate("button", "Save"), candidate("link", "Save")];
        let resolved = ("button".into(), "Save".into());
        assert_eq!(match_resolved(&candidates, &resolved), Some(0));
    }

    #[test]
    fn case_insensitive_name_match() {
        let candidates = vec![candidate("button", "Create Customer")];
        let resolved = ("button".into(), "create customer".into());
        assert_eq!(match_resolved(&candidates, &resolved), Some(0));
    }

    #[test]
    fn empty_resolved_role_falls_back_to_name_only() {
        let candidates = vec![candidate("combobox", "Plan")];
        let resolved = (String::new(), "Plan".into());
        assert_eq!(match_resolved(&candidates, &resolved), Some(0));
    }

    #[test]
    fn empty_resolved_name_is_no_match() {
        let candidates = vec![candidate("button", "Save")];
        let resolved = ("button".into(), String::new());
        assert_eq!(match_resolved(&candidates, &resolved), None);
    }

    #[test]
    fn wrong_role_falls_back_to_name_only_match() {
        let candidates = vec![candidate("button", "Save")];
        let resolved = ("link".into(), "Save".into());
        assert_eq!(match_resolved(&candidates, &resolved), Some(0));
    }

    #[test]
    fn no_match_returns_none() {
        let candidates = vec![candidate("button", "Save")];
        let resolved = ("button".into(), "Delete".into());
        assert_eq!(match_resolved(&candidates, &resolved), None);
    }

    #[test]
    fn inspect_counts_records_and_leaves_bytes_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vision-corpus.jsonl");
        std::fs::write(&path, "{\"ok\":true}\n{\"ok\":true}\n").unwrap();
        let before = std::fs::read(&path).unwrap();
        let health = VisionCorpus::inspect(&path).unwrap();
        assert_eq!(health.records, 2);
        assert!(!health.torn_tail);
        assert_eq!(health.corrupt_line, None);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn inspect_missing_corpus_is_empty_health() {
        let dir = tempfile::tempdir().unwrap();
        let health = VisionCorpus::inspect(dir.path().join("nope.jsonl")).unwrap();
        assert!(!health.exists);
        assert_eq!(health.bytes, 0);
    }

    #[test]
    fn corpus_url_drops_credentials_query_fragment_and_dynamic_ids() {
        assert_eq!(
            sanitize_corpus_url(
                "https://alice:secret@example.com/accounts/550e8400-e29b-41d4-a716-446655440000/edit?token=secret#recovery"
            ),
            Some("https://example.com/accounts/:id/edit".into())
        );
        assert_eq!(
            sanitize_corpus_url("https://example.com/reset/abc123"),
            Some("https://example.com/reset/:id".into())
        );
    }

    #[test]
    fn corpus_labels_redact_embedded_contact_and_credential_values() {
        assert_eq!(
            sanitize_corpus_label("Enter 'maya@atlas.example' in the email field"),
            "Enter [redacted] in the email field"
        );
        assert_eq!(
            sanitize_corpus_label("Use Bearer sk-live-secret"),
            "[redacted]"
        );
    }

    #[test]
    fn corpus_actions_never_serialize_runtime_payloads() {
        for action in [
            VisionAction::TypeText {
                text: "must-not-survive".into(),
            },
            VisionAction::ExtractValue {
                value: "must-not-survive".into(),
            },
        ] {
            let encoded = raw_action(&action, Some(2)).to_string();
            assert!(!encoded.contains("must-not-survive"));
            assert_eq!(encoded.parse::<serde_json::Value>().unwrap()["index"], 2);
        }
    }

    #[cfg(unix)]
    #[test]
    fn corpus_storage_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("corpus");
        let corpus = VisionCorpus::new(&dir).unwrap();
        let record = CorpusRecord {
            image_b64: "c2FuaXRpemVk".into(),
            purpose: "Find password input".into(),
            intent_kind: "fill".into(),
            stuck: "targetMissing".into(),
            context_url: Some("https://example.com/login?token=secret".into()),
            context_candidates: vec![candidate("textbox", "Password")],
            target_index: Some(0),
            resolved_element: None,
            model_response: CorpusModelResponse {
                confidence: 1.0,
                action: serde_json::json!({"kind":"typeText","text":"secret"}),
            },
            success: true,
            journey: "production".into(),
            step: "fill".into(),
            outcome_stage: "visionFallback".into(),
            error_message: Some("Authorization: Bearer secret".into()),
        };
        corpus.record(&record);

        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        let path = dir.join("vision-corpus.jsonl");
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let stored = std::fs::read_to_string(path).unwrap();
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
        assert!(stored.contains("\"privacyVersion\":1"));
        assert!(stored.contains("https://example.com/login"));
        assert!(!stored.contains("secret"));
        assert!(!stored.contains("errorMessage"));
    }
}
