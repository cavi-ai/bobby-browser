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
    pub passed: bool,
    pub tool_calls: u64,
    pub wall_ms: u64,
    pub snapshots_taken: u64,
    pub vision_escalations_attempted: u64,
    pub vision_escalations_accepted: u64,
    pub failed_commands: u64,
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
            passed,
            tool_calls,
            wall_ms,
            snapshots_taken,
            vision_escalations_attempted,
            vision_escalations_accepted,
            failed_commands,
        })
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
