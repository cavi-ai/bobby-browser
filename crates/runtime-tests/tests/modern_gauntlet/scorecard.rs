use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Scorecard {
    pub station: String,
    pub engine: String,
    pub provider_mode: ProviderMode,
    pub model_tier: ModelTier,
    pub context_source: ContextSource,
    pub vision_source: VisionSource,
    pub passed: bool,
    pub tool_calls: u64,
    pub action_count: u64,
    pub wall_ms: u64,
    pub snapshots_taken: u64,
    pub vision_escalations_attempted: u64,
    pub vision_escalations_accepted: u64,
    pub failed_commands: u64,
    pub failure_taxonomy: FailureTaxonomy,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ProviderMode {
    Http,
    Acp,
    DirectLocal,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ModelTier {
    Deterministic,
    Vision,
    Hybrid,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ContextSource {
    None,
    Live,
    Persisted,
    Mixed,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum VisionSource {
    None,
    Prefill,
    Fallback,
    Mixed,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FailureTaxonomy {
    pub timeout: u64,
    pub resolution: u64,
    pub policy: u64,
    pub provider: u64,
    pub reconciliation: u64,
    pub other: u64,
}

#[derive(Debug)]
pub struct ScorecardError(String);

impl Display for ScorecardError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ScorecardError {}

#[derive(Default)]
struct CommandStats {
    accepted_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    command_kind: Option<String>,
    failed: bool,
    failure_code: Option<String>,
    vision_attempted: u64,
    vision_accepted: u64,
}

impl Scorecard {
    pub fn from_journal(
        station: impl Into<String>,
        engine: impl Into<String>,
        path: &Path,
        passed: bool,
    ) -> Result<Self, ScorecardError> {
        Self::from_journal_with_environment(
            station,
            engine,
            ProviderMode::Unknown,
            ModelTier::Unknown,
            path,
            passed,
        )
    }

    pub fn from_journal_with_environment(
        station: impl Into<String>,
        engine: impl Into<String>,
        provider_mode: ProviderMode,
        model_tier: ModelTier,
        path: &Path,
        passed: bool,
    ) -> Result<Self, ScorecardError> {
        let contents = std::fs::read_to_string(path).map_err(|error| {
            ScorecardError(format!(
                "failed to read journal {}: {error}",
                path.display()
            ))
        })?;
        let mut commands = BTreeMap::<String, CommandStats>::new();

        for (line_index, line) in contents.lines().enumerate() {
            let line_number = line_index + 1;
            let record: Value = serde_json::from_str(line).map_err(|error| {
                ScorecardError(format!(
                    "journal line {line_number} is invalid JSON: {error}"
                ))
            })?;
            let Some(command_id) = record.get("commandId").and_then(Value::as_str) else {
                continue;
            };
            let phase = record
                .get("phase")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if phase != "accepted" && phase != "completed" && phase != "failed" {
                continue;
            }
            let timestamp = parse_timestamp(&record, line_number)?;
            let command = commands.entry(command_id.to_owned()).or_default();

            match phase {
                "accepted" => {
                    command.accepted_at = Some(timestamp);
                    command.command_kind = command_kind(&record).map(str::to_owned);
                }
                "completed" | "failed" => {
                    command.completed_at = Some(timestamp);
                    command.failed = phase == "failed";
                    command.failure_code = failure_code(&record).map(str::to_owned);
                    if phase == "completed" {
                        let (attempted, accepted) = vision_counts(&record);
                        command.vision_attempted += attempted;
                        command.vision_accepted += accepted;
                    }
                }
                _ => unreachable!(),
            }
        }

        let tool_calls = commands.len() as u64;
        let failed_commands = commands.values().filter(|command| command.failed).count() as u64;
        let action_count = commands
            .values()
            .filter(|command| command.command_kind.as_deref().is_some_and(is_action_kind))
            .count() as u64;
        let failure_taxonomy = commands.values().filter(|command| command.failed).fold(
            FailureTaxonomy::default(),
            |mut taxonomy, command| {
                taxonomy.record(command.failure_code.as_deref());
                taxonomy
            },
        );
        let snapshots_taken = commands
            .values()
            .filter(|command| {
                matches!(
                    command.command_kind.as_deref(),
                    Some("captureScreenshot" | "accessibilitySnapshot" | "formSnapshot")
                )
            })
            .count() as u64;
        let vision_escalations_attempted = commands
            .values()
            .map(|command| command.vision_attempted)
            .sum();
        let vision_escalations_accepted = commands
            .values()
            .map(|command| command.vision_accepted)
            .sum();
        let (context_source, vision_source) = source_summary(&contents);
        let (started_at, ended_at) = commands
            .values()
            .filter_map(|command| command.accepted_at.zip(command.completed_at))
            .fold(
                (None::<DateTime<Utc>>, None::<DateTime<Utc>>),
                |(started, ended), (command_started, command_ended)| {
                    (
                        Some(started.map_or(command_started, |value| value.min(command_started))),
                        Some(ended.map_or(command_ended, |value| value.max(command_ended))),
                    )
                },
            );
        let wall_ms = started_at
            .zip(ended_at)
            .map(|(started, ended)| (ended - started).num_milliseconds().max(0) as u64)
            .unwrap_or_default();

        Ok(Self {
            station: station.into(),
            engine: engine.into(),
            provider_mode,
            model_tier,
            context_source,
            vision_source,
            passed,
            tool_calls,
            action_count,
            wall_ms,
            snapshots_taken,
            vision_escalations_attempted,
            vision_escalations_accepted,
            failed_commands,
            failure_taxonomy,
        })
    }
}

impl ProviderMode {
    pub fn from_label(value: &str) -> Self {
        match value {
            "http" => Self::Http,
            "acp" => Self::Acp,
            "direct-local" | "directLocal" => Self::DirectLocal,
            _ => Self::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Acp => "acp",
            Self::DirectLocal => "direct-local",
            Self::Unknown => "unknown",
        }
    }
}

impl ModelTier {
    pub fn from_label(value: &str) -> Self {
        match value {
            "deterministic" => Self::Deterministic,
            "vision" => Self::Vision,
            "hybrid" => Self::Hybrid,
            _ => Self::Unknown,
        }
    }
}

impl FailureTaxonomy {
    fn record(&mut self, code: Option<&str>) {
        let normalized = code.unwrap_or_default().to_ascii_lowercase();
        if normalized.contains("timeout") || normalized.contains("deadline") {
            self.timeout += 1;
        } else if normalized.contains("target") || normalized.contains("selector") {
            self.resolution += 1;
        } else if normalized.contains("policy") || normalized.contains("denied") {
            self.policy += 1;
        } else if normalized.contains("vision") || normalized.contains("provider") {
            self.provider += 1;
        } else if normalized.contains("reconcil") || normalized.contains("checkpoint") {
            self.reconciliation += 1;
        } else {
            self.other += 1;
        }
    }
}

fn is_action_kind(kind: &str) -> bool {
    !matches!(
        kind,
        "captureScreenshot" | "accessibilitySnapshot" | "formSnapshot" | "inspect" | "waitFor"
    )
}

fn failure_code(record: &Value) -> Option<&str> {
    record
        .get("outcome")
        .and_then(|outcome| outcome.get("error"))
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
}

fn source_summary(contents: &str) -> (ContextSource, VisionSource) {
    let mut live_context = false;
    let mut persisted_context = false;
    let mut prefill = false;
    let mut fallback = false;
    for record in contents
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
    {
        visit_source_values(
            &record,
            &mut live_context,
            &mut persisted_context,
            &mut prefill,
            &mut fallback,
        );
    }
    let context = match (live_context, persisted_context) {
        (false, false) => ContextSource::None,
        (true, false) => ContextSource::Live,
        (false, true) => ContextSource::Persisted,
        (true, true) => ContextSource::Mixed,
    };
    let vision = match (prefill, fallback) {
        (false, false) => VisionSource::None,
        (true, false) => VisionSource::Prefill,
        (false, true) => VisionSource::Fallback,
        (true, true) => VisionSource::Mixed,
    };
    (context, vision)
}

fn visit_source_values(
    value: &Value,
    live_context: &mut bool,
    persisted_context: &mut bool,
    prefill: &mut bool,
    fallback: &mut bool,
) {
    match value {
        Value::Object(object) => {
            if let Some(source) = object.get("contextSource").and_then(Value::as_str) {
                *live_context |= source == "live";
                *persisted_context |= source == "persisted" || source == "retained";
            }
            if let Some(path) = object.get("resolutionPath").and_then(Value::as_str) {
                *prefill |= path == "visionPrefill";
                *fallback |= path == "visionFallback";
            }
            for child in object.values() {
                visit_source_values(child, live_context, persisted_context, prefill, fallback);
            }
        }
        Value::Array(items) => {
            for child in items {
                visit_source_values(child, live_context, persisted_context, prefill, fallback);
            }
        }
        _ => {}
    }
}

fn parse_timestamp(record: &Value, line_number: usize) -> Result<DateTime<Utc>, ScorecardError> {
    let value = record
        .get("recordedAt")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ScorecardError(format!("journal line {line_number} is missing recordedAt"))
        })?;
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|error| {
            ScorecardError(format!(
                "journal line {line_number} has invalid recordedAt {value:?}: {error}"
            ))
        })
}

fn command_kind(record: &Value) -> Option<&str> {
    let command = record.get("envelope")?.get("command")?;
    if command.get("kind")?.as_str()? == "primitive" {
        command.get("input")?.get("kind")?.as_str()
    } else {
        command.get("kind")?.as_str()
    }
}

fn vision_counts(record: &Value) -> (u64, u64) {
    let Some(evidence) = record
        .get("outcome")
        .and_then(|outcome| outcome.get("evidence"))
        .and_then(Value::as_array)
    else {
        return (0, 0);
    };

    evidence.iter().fold((0, 0), |(attempted, accepted), item| {
        let path = item
            .get("record")
            .and_then(|record| record.get("resolutionPath"))
            .or_else(|| item.get("resolutionPath"))
            .and_then(Value::as_str);
        let is_vision = matches!(path, Some("visionFallback" | "visionPrefill"));
        if !is_vision {
            return (attempted, accepted);
        }
        let verification = item
            .get("record")
            .and_then(|record| record.get("verification"))
            .and_then(Value::as_str);
        let accepted = accepted
            + u64::from(!matches!(
                verification,
                Some("targetNotFound" | "targetAmbiguous" | "obstructionPersisted")
            ));
        (attempted + 1, accepted)
    })
}

#[cfg(test)]
mod tests {
    use super::command_kind;
    use serde_json::json;

    #[test]
    fn command_kind_unwraps_primitive_envelopes() {
        let record = json!({
            "envelope": {
                "command": {
                    "kind": "primitive",
                    "input": { "kind": "captureScreenshot" }
                }
            }
        });

        assert_eq!(command_kind(&record), Some("captureScreenshot"));
    }
}
