// Two test targets include this module and each uses a different subset:
// modern_gauntlet_scorecard calls from_journal, modern_gauntlet_e2e builds every
// journal through from_journal_with_environment and reads the environment labels.
// Under `-D warnings` each target rejects the half it does not call.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
    /// From journey launch through scorecard capture, including browser setup.
    pub journey_wall_ms: u64,
    /// Sum of complete command outcomes returned to the direct Rust journey caller.
    pub serialized_response_bytes: u64,
    pub snapshots_taken: u64,
    pub vision_escalations_attempted: u64,
    pub vision_escalations_accepted: u64,
    pub failed_commands: u64,
    pub failure_taxonomy: FailureTaxonomy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JourneyBudget {
    pub max_tool_calls: u64,
    pub max_action_count: u64,
    pub max_snapshots: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ProviderMode {
    Http,
    Acp,
    DirectLocal,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ModelTier {
    Deterministic,
    Vision,
    Hybrid,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ContextSource {
    None,
    Live,
    Persisted,
    Mixed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum VisionSource {
    None,
    Prefill,
    Fallback,
    Mixed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
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
            journey_wall_ms: 0,
            serialized_response_bytes: 0,
            snapshots_taken,
            vision_escalations_attempted,
            vision_escalations_accepted,
            failed_commands,
            failure_taxonomy,
        })
    }

    pub fn enforce_release_budget(&self) -> Result<(), ScorecardError> {
        let budget = release_budget_for(&self.station, &self.engine).ok_or_else(|| {
            ScorecardError(format!(
                "no release budget for station={} engine={}",
                self.station, self.engine
            ))
        })?;
        let mut violations = Vec::new();
        if self.journey_wall_ms == 0 {
            violations.push("journeyWallMs missing".to_string());
        }
        if self.serialized_response_bytes == 0 {
            violations.push("serializedResponseBytes missing".to_string());
        }
        if !self.passed {
            violations.push("passed=false".to_string());
        }
        if self.tool_calls > budget.max_tool_calls {
            violations.push(format!(
                "toolCalls={}>{}",
                self.tool_calls, budget.max_tool_calls
            ));
        }
        if self.action_count > budget.max_action_count {
            violations.push(format!(
                "actionCount={}>{}",
                self.action_count, budget.max_action_count
            ));
        }
        if self.snapshots_taken > budget.max_snapshots {
            violations.push(format!(
                "snapshotsTaken={}>{}",
                self.snapshots_taken, budget.max_snapshots
            ));
        }
        if self.failed_commands != 0 {
            violations.push(format!("failedCommands={}", self.failed_commands));
        }
        if self.vision_escalations_accepted > self.vision_escalations_attempted {
            violations.push(format!(
                "visionAccepted={}>visionAttempted={}",
                self.vision_escalations_accepted, self.vision_escalations_attempted
            ));
        }
        if self.vision_escalations_attempted > 0 && self.vision_source == VisionSource::None {
            violations.push("visionSource=none with attempted escalation".to_string());
        }
        if violations.is_empty() {
            Ok(())
        } else {
            Err(ScorecardError(format!(
                "release budget exceeded for station={} engine={}: {}",
                self.station,
                self.engine,
                violations.join(", ")
            )))
        }
    }
}

pub fn release_budget_for(station: &str, engine: &str) -> Option<JourneyBudget> {
    if !matches!(engine, "chromium" | "firefox") {
        return None;
    }
    let budget = match station {
        "session" => JourneyBudget {
            max_tool_calls: 11,
            max_action_count: 8,
            max_snapshots: 1,
        },
        "customer-update" => JourneyBudget {
            max_tool_calls: 25,
            max_action_count: 16,
            max_snapshots: 2,
        },
        "onboarding" => JourneyBudget {
            max_tool_calls: 30,
            max_action_count: 21,
            max_snapshots: 2,
        },
        "documents" => JourneyBudget {
            max_tool_calls: 22,
            max_action_count: 12,
            max_snapshots: 2,
        },
        "authorization" => JourneyBudget {
            max_tool_calls: 22,
            max_action_count: 14,
            max_snapshots: 2,
        },
        "checkout" => JourneyBudget {
            max_tool_calls: 32,
            max_action_count: 21,
            max_snapshots: 2,
        },
        "report-recovery" => JourneyBudget {
            max_tool_calls: 30,
            max_action_count: 18,
            max_snapshots: 3,
        },
        _ => return None,
    };
    Some(budget)
}

pub fn enforce_remembered_site_reduction(
    station: &str,
    cold_calls: usize,
    remembered_calls: usize,
) -> Result<(), ScorecardError> {
    if remembered_calls < cold_calls {
        Ok(())
    } else {
        Err(ScorecardError(format!(
            "remembered-site call budget exceeded for station={station}: rememberedCalls={remembered_calls}, coldCalls={cold_calls}"
        )))
    }
}

/// Compare matching 20-run measured cohorts after two discarded warmups per
/// cohort. The wall limit is twice baseline p95; the response-byte limit is
/// 110% of the largest baseline run. No missing sample is treated as zero.
pub fn enforce_measured_regression(
    baseline: &[Scorecard],
    candidate: &[Scorecard],
) -> Result<(), ScorecardError> {
    if baseline.len() != 20 || candidate.len() != 20 {
        return Err(ScorecardError(
            "baseline and candidate each require 20 measured runs after warmup".into(),
        ));
    }
    let reference = &baseline[0];
    if baseline.iter().chain(candidate).any(|sample| {
        !sample.passed
            || sample.station != reference.station
            || sample.engine != reference.engine
            || sample.provider_mode != reference.provider_mode
            || sample.journey_wall_ms == 0
            || sample.serialized_response_bytes == 0
    }) {
        return Err(ScorecardError(
            "measured runs must pass and share station, engine, and provider mode".into(),
        ));
    }
    let p95 = |samples: &[Scorecard]| {
        let mut walls = samples
            .iter()
            .map(|sample| sample.journey_wall_ms)
            .collect::<Vec<_>>();
        walls.sort_unstable();
        walls[18]
    };
    let baseline_p95 = p95(baseline);
    let candidate_p95 = p95(candidate);
    if candidate_p95 > baseline_p95.saturating_mul(2) {
        return Err(ScorecardError(format!(
            "journeyWallMs p95={candidate_p95} exceeds 2x baseline p95={baseline_p95}"
        )));
    }
    let baseline_bytes = baseline
        .iter()
        .map(|sample| sample.serialized_response_bytes)
        .max()
        .unwrap();
    let candidate_bytes = candidate
        .iter()
        .map(|sample| sample.serialized_response_bytes)
        .max()
        .unwrap();
    if u128::from(candidate_bytes) * 10 > u128::from(baseline_bytes) * 11 {
        return Err(ScorecardError(format!(
            "serializedResponseBytes max={candidate_bytes} exceeds 110% of baseline max={baseline_bytes}"
        )));
    }
    Ok(())
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
