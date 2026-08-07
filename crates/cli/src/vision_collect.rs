use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine;
use serde::Serialize;
use sha2::{Digest, Sha256};

use vision_proxy::{DataCollectorConfig, ProposeInput, ProposeResponse, VisionAction, VisionDataCollector};

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
        std::fs::create_dir_all(&config.output_dir)
            .context("failed to create output directory")?;

        let data_collector = Arc::new(VisionDataCollector::new(
            DataCollectorConfig {
                output_dir: config.output_dir.clone(),
                enabled: config.enabled,
                flush_interval_ms: config.flush_interval_ms,
            },
        ));

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
            self.config.output_dir.file_stem().unwrap_or_default().to_str().unwrap_or("run").to_string(),
            "llava:7b".to_string(),
        );

        // Also log to data collector for real-time collection
        let _ = self.data_collector.log_proposal(
            example.image_b64.clone(),
            &ProposeInput {
                purpose: example.purpose.clone(),
                intent_kind: example.intent_kind.clone(),
                stuck: example.stuck.clone(),
                screenshot_png_b64: example.image_b64.clone(),
                context: None, // Would need to extract from context
            },
            example.model_response.as_ref().and_then(|r| {
                // Convert to ProposeResponse
                let action = match r.get("action") {
                    Some(action) => match action.get("kind") {
                        Some(kind) if kind.as_str() == Some("click") => {
                            let x = action.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
                            let y = action.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
                            Some(VisionAction::Click { x, y })
                        }
                        Some(kind) if kind.as_str() == Some("typeText") => {
                            let text = action.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            Some(VisionAction::TypeText { text })
                        }
                        Some(kind) if kind.as_str() == Some("extractValue") => {
                            let value = action.get("value").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            Some(VisionAction::ExtractValue { value })
                        }
                        _ => None,
                    },
                    None => None,
                };
                let confidence = r.get("confidence").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                action.map(|a| ProposeResponse { confidence, action: a })
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
            file.write_all(b"\n")
                .context("failed to write newline")?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(example.image_hash.len() > 0);
    }
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

    // Create collector
    let mut collector = GauntletDataCollector::new(config)?;

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
    println!("  python3 scripts/vision-mlx/bobby_vision_collector.py --generate --num-examples 1000");

    Ok(())
}
