//! Training-data collection for the vision gauntlet.
//!
//! The collector API is staged ahead of the runner that will drive it: the
//! command below creates and validates the output directory and prints the
//! collection instructions, while `GauntletDataCollector`'s accessors and
//! `save`/`stats` wait on the gauntlet integration.
#![allow(dead_code)]

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use vision_proxy::{
    DataCollectorConfig, ProposeInput, ProposeResponse, VisionAction, VisionDataCollector,
};

/// Configuration for the gauntlet data collection CLI tool.
#[derive(Debug, Clone)]
pub struct CollectConfig {
    /// Output directory for collected data (default: "data/vision/")
    pub output_dir: PathBuf,
    /// Enable data collection (default: true)
    pub enabled: bool,
    /// Collection interval in milliseconds (default: 1000)
    pub flush_interval_ms: u64,
    /// Number of examples to collect per journey (default: 100)
    pub examples_per_journey: usize,
    /// Specific journey to collect (default: all)
    pub journey: Option<String>,
}

impl Default for CollectConfig {
    fn default() -> Self {
        Self {
            output_dir: PathBuf::from("data/vision/"),
            enabled: true,
            flush_interval_ms: 1000,
            examples_per_journey: 100,
            journey: None,
        }
    }
}

/// A single training example collected from a gauntlet run.
#[derive(Debug, Clone, Serialize)]
pub struct GauntletTrainingExample {
    /// Base64 encoded PNG screenshot
    pub image_b64: String,
    /// User's stated purpose (e.g., "Fill login form")
    pub purpose: String,
    /// Intent type (locate, typeText, extractValue, etc.)
    pub intent_kind: String,
    /// Stuck reason (targetMissing, targetAmbiguous, etc.)
    pub stuck: String,
    /// Optional context: URL, candidates, recent commands
    pub context: Option<serde_json::Value>,
    /// Model's response (confidence + action)
    pub model_response: Option<serde_json::Value>,
    /// Whether the action succeeded (set by runtime)
    pub success: bool,
    /// Gauntlet journey name
    pub journey: String,
    /// Step within journey
    pub step: String,
    /// Optional error message
    pub error_message: Option<String>,
    /// Timestamp
    pub timestamp: String,
    /// Run ID (groups examples from same run)
    pub run_id: String,
    /// Model name used
    pub model_name: String,
    /// SHA256 of image for deduplication
    pub image_hash: String,
}

impl GauntletTrainingExample {
    // A flat training record: one argument per field, as in
    // `VisionTrainingExample::new`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        screenshot_png_b64: String,
        purpose: String,
        intent_kind: String,
        stuck: String,
        context: Option<serde_json::Value>,
        model_response: Option<serde_json::Value>,
        success: bool,
        journey: String,
        step: String,
        error_message: Option<String>,
        run_id: String,
        model_name: String,
    ) -> Self {
        // Compute image hash
        let image_hash = if !screenshot_png_b64.is_empty() {
            let bytes = base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                &screenshot_png_b64,
            )
            .unwrap_or_default();
            let hash = Sha256::digest(&bytes);
            format!("{:02x?}", hash).replace([' ', ':'], "")
        } else {
            "unknown".to_string()
        };

        Self {
            image_b64: screenshot_png_b64,
            purpose,
            intent_kind,
            stuck,
            context,
            model_response,
            success,
            journey,
            step,
            error_message,
            timestamp: chrono::Utc::now().to_rfc3339(),
            run_id,
            model_name,
            image_hash,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap()
    }
}

/// Collects training data from gauntlet runs.
pub struct GauntletDataCollector {
    config: CollectConfig,
    data_collector: Arc<VisionDataCollector>,
    output_file: Option<PathBuf>,
    examples: Vec<GauntletTrainingExample>,
}

impl GauntletDataCollector {
    pub fn new(config: CollectConfig) -> Result<Self> {
        // Create output directory
        std::fs::create_dir_all(&config.output_dir).context("failed to create output directory")?;

        let data_collector = Arc::new(VisionDataCollector::new(DataCollectorConfig {
            output_dir: config.output_dir.clone(),
            enabled: config.enabled,
            flush_interval_ms: config.flush_interval_ms,
        }));

        Ok(Self {
            config,
            data_collector,
            output_file: None,
            examples: Vec::new(),
        })
    }

    pub fn config(&self) -> &CollectConfig {
        &self.config
    }

    pub fn data_collector(&self) -> &Arc<VisionDataCollector> {
        &self.data_collector
    }

    /// Collect a single training example from a gauntlet step.
    // Forwards the same flat record.
    #[allow(clippy::too_many_arguments)]
    pub fn collect_example(
        &mut self,
        screenshot_png_b64: String,
        purpose: String,
        intent_kind: String,
        stuck: String,
        context: Option<serde_json::Value>,
        model_response: Option<serde_json::Value>,
        success: bool,
        journey: String,
        step: String,
        error_message: Option<String>,
    ) -> Result<GauntletTrainingExample> {
        let example = GauntletTrainingExample::new(
            screenshot_png_b64,
            purpose,
            intent_kind,
            stuck,
            context,
            model_response,
            success,
            journey,
            step,
            error_message,
            self.config
                .output_dir
                .file_stem()
                .unwrap_or_default()
                .to_str()
                .unwrap_or("run")
                .to_string(),
            "llava:7b".to_string(),
        );

        // Also log to data collector for real-time collection
        self.data_collector.log_proposal(
            example.image_b64.clone(),
            &ProposeInput {
                purpose: example.purpose.clone(),
                intent_kind: example.intent_kind.clone(),
                stuck: example.stuck.clone(),
                screenshot_png_b64: example.image_b64.clone(),
                context: None, // Would need to extract from context
            },
            example.model_response.as_ref().and_then(|r| {
                let action = r.get("action").and_then(parse_model_action);
                let confidence = r.get("confidence").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                action.map(|a| ProposeResponse {
                    confidence,
                    action: a,
                })
            }),
            Some(example.journey.clone()),
            Some(example.step.clone()),
            Some(example.success),
            example.error_message.clone(),
            Some(example.run_id.clone()),
            Some(example.model_name.clone()),
        );

        self.examples.push(example.clone());
        Ok(example)
    }

    /// Save collected examples to disk.
    pub fn save(&self) -> Result<PathBuf> {
        let output_path = self.config.output_dir.join("training_data.jsonl");

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&output_path)
            .context("failed to open training data file")?;

        for example in &self.examples {
            let json = example.to_json();
            file.write_all(json.as_bytes())
                .context("failed to write training data")?;
            file.write_all(b"\n").context("failed to write newline")?;
        }

        // Print summary
        let total = self.examples.len();
        let success = self.examples.iter().filter(|e| e.success).count();
        let failed = total - success;

        println!("\n=== Training Dataset Summary ===");
        println!("Total examples: {total}");
        println!("Success: {success}");
        println!("Failed: {failed}");
        println!("\nDataset saved to: {}", output_path.display());

        Ok(output_path)
    }

    /// Get collection statistics.
    pub fn stats(&self) -> (usize, usize, usize) {
        let total = self.examples.len();
        let success = self.examples.iter().filter(|e| e.success).count();
        let failed = total - success;
        (total, success, failed)
    }
}

fn parse_model_action(action: &serde_json::Value) -> Option<VisionAction> {
    let kind = action.get("kind")?.as_str()?;
    match kind {
        "click" => Some(VisionAction::Click {
            x: action
                .get("x")
                .and_then(|value| value.as_f64())
                .unwrap_or(0.0),
            y: action
                .get("y")
                .and_then(|value| value.as_f64())
                .unwrap_or(0.0),
        }),
        "typeText" => Some(VisionAction::TypeText {
            text: action
                .get("text")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string(),
        }),
        "extractValue" => Some(VisionAction::ExtractValue {
            value: action
                .get("value")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string(),
        }),
        "clickCandidate" => Some(VisionAction::ClickCandidate {
            index: candidate_index(action)?,
        }),
        "typeIntoCandidate" => Some(VisionAction::TypeIntoCandidate {
            index: candidate_index(action)?,
        }),
        "extractFromCandidate" => Some(VisionAction::ExtractFromCandidate {
            index: candidate_index(action)?,
        }),
        _ => None,
    }
}

fn candidate_index(action: &serde_json::Value) -> Option<u32> {
    action
        .get("index")?
        .as_u64()
        .and_then(|index| u32::try_from(index).ok())
}

/// Run the training data collection CLI command.
pub fn run_collect(
    output: String,
    examples_per_journey: usize,
    journey: Option<String>,
) -> Result<()> {
    let config = CollectConfig {
        output_dir: PathBuf::from(output),
        enabled: true,
        flush_interval_ms: 1000,
        examples_per_journey,
        journey,
    };

    println!("Starting training data collection...");
    println!("Output directory: {}", config.output_dir.display());
    println!("Examples per journey: {}", config.examples_per_journey);

    // Constructed for the side effect: `new` creates the output directory and
    // fails if it cannot, so the path printed above is validated before the
    // instructions below tell the operator to fill it.
    let _collector = GauntletDataCollector::new(config)?;

    // In production, this would:
    // 1. Launch the gauntlet scenario server
    // 2. Run each journey with vision assist enabled
    // 3. Capture vision proposals and outcomes
    // 4. Store as JSONL

    // For now, print instructions
    println!("\nTo collect real training data:");
    println!("1. Ensure Ollama is running with llava:7b");
    println!("2. Run: bobby serve --vision (enables vision assist)");
    println!("3. Run gauntlet tests with vision enabled");
    println!("4. Data will be collected automatically by the vision proxy");
    println!("\nOr use the Python collector:");
    println!(
        "  python3 scripts/vision-mlx/bobby_vision_collector.py --generate --num-examples 1000"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_action_parser_requires_a_u32_index() {
        for (kind, expected) in [
            ("clickCandidate", VisionAction::ClickCandidate { index: 7 }),
            (
                "typeIntoCandidate",
                VisionAction::TypeIntoCandidate { index: 7 },
            ),
            (
                "extractFromCandidate",
                VisionAction::ExtractFromCandidate { index: 7 },
            ),
        ] {
            let action = serde_json::json!({"kind": kind, "index": 7});
            let actual = parse_model_action(&action).expect("valid candidate action");
            assert!(matches!(
                (expected, actual),
                (
                    VisionAction::ClickCandidate { index: 7 },
                    VisionAction::ClickCandidate { index: 7 }
                ) | (
                    VisionAction::TypeIntoCandidate { index: 7 },
                    VisionAction::TypeIntoCandidate { index: 7 }
                ) | (
                    VisionAction::ExtractFromCandidate { index: 7 },
                    VisionAction::ExtractFromCandidate { index: 7 }
                )
            ));
        }

        for index in [
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!(u64::from(u32::MAX) + 1),
        ] {
            assert!(parse_model_action(&serde_json::json!({
                "kind": "typeIntoCandidate",
                "index": index,
            }))
            .is_none());
        }
    }

    #[test]
    fn collector_writes_candidate_actions_to_the_secondary_dataset() {
        let temp = tempfile::tempdir().unwrap();
        let mut collector = GauntletDataCollector::new(CollectConfig {
            output_dir: temp.path().join("vision"),
            flush_interval_ms: 0,
            ..CollectConfig::default()
        })
        .unwrap();
        for kind in [
            "clickCandidate",
            "typeIntoCandidate",
            "extractFromCandidate",
        ] {
            collector
                .collect_example(
                    "dGVzdA==".into(),
                    "target field".into(),
                    "fill".into(),
                    "targetMissing".into(),
                    None,
                    Some(serde_json::json!({
                        "confidence": 0.9,
                        "action": {"kind": kind, "index": 7},
                    })),
                    true,
                    "journey".into(),
                    "step".into(),
                    None,
                )
                .unwrap();
        }

        let records = std::fs::read_to_string(temp.path().join("vision/training_data.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 3);
        for (record, kind) in records.iter().zip([
            "clickCandidate",
            "typeIntoCandidate",
            "extractFromCandidate",
        ]) {
            assert_eq!(record["model_response"]["action"]["kind"], kind);
            assert_eq!(record["model_response"]["action"]["index"], 7);
        }
    }

    #[test]
    fn test_example_creation() {
        let example = GauntletTrainingExample::new(
            "dGVzdA==".to_string(), // base64 "test"
            "test".to_string(),
            "locate".to_string(),
            "targetMissing".to_string(),
            None,
            None,
            true,
            "test-journey".to_string(),
            "step_0".to_string(),
            None,
            "test-run".to_string(),
            "llava:7b".to_string(),
        );

        assert_eq!(example.purpose, "test");
        assert_eq!(example.intent_kind, "locate");
        assert!(!example.image_hash.is_empty());
    }
}
