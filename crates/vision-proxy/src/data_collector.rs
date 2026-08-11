use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::upstream::ProposeInput;
use crate::wire::ProposeResponse;

/// Configuration for the vision data collector.
#[derive(Debug, Clone)]
pub struct DataCollectorConfig {
    /// Output directory for collected data (default: "data/vision/")
    pub output_dir: PathBuf,
    /// Enable data collection (default: false)
    pub enabled: bool,
    /// Collection interval in milliseconds (default: 1000)
    pub flush_interval_ms: u64,
}

impl Default for DataCollectorConfig {
    fn default() -> Self {
        Self {
            output_dir: PathBuf::from("data/vision/"),
            enabled: false,
            flush_interval_ms: 1000,
        }
    }
}

/// A single training example collected from a vision proposal.
#[derive(Debug, Serialize)]
pub struct VisionTrainingExample {
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
    pub success: Option<bool>,
    /// Gauntlet journey name
    pub journey: Option<String>,
    /// Step within journey
    pub step: Option<String>,
    /// Optional error message
    pub error_message: Option<String>,
    /// Timestamp
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// Run ID (groups examples from same run)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Model name used
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    /// SHA256 of image for deduplication
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_hash: Option<String>,
}

impl VisionTrainingExample {
    // A flat training record: every argument is one of the struct's own
    // fields, so the arity is the record's width rather than a signature
    // that wants splitting.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        screenshot_png_b64: String,
        input: &ProposeInput,
        response: Option<ProposeResponse>,
        journey: Option<String>,
        step: Option<String>,
        success: Option<bool>,
        error_message: Option<String>,
        run_id: Option<String>,
        model_name: Option<String>,
    ) -> Self {
        // Compute image hash
        let image_hash = if !screenshot_png_b64.is_empty() {
            let bytes = base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                &screenshot_png_b64,
            )
            .unwrap_or_default();
            let hash = Sha256::digest(&bytes);
            Some(format!("{:02x?}", hash).replace([' ', ':'], ""))
        } else {
            None
        };

        // Extract model response
        let model_response = response.map(|r| {
            let action = match &r.action {
                crate::wire::VisionAction::Click { x, y } => {
                    serde_json::json!({"kind": "click", "x": x, "y": y})
                }
                crate::wire::VisionAction::TypeText { text } => {
                    serde_json::json!({"kind": "typeText", "text": text})
                }
                crate::wire::VisionAction::ExtractValue { value } => {
                    serde_json::json!({"kind": "extractValue", "value": value})
                }
                crate::wire::VisionAction::ClickCandidate { index } => {
                    serde_json::json!({"kind": "clickCandidate", "index": index})
                }
                crate::wire::VisionAction::TypeIntoCandidate { index } => {
                    serde_json::json!({"kind": "typeIntoCandidate", "index": index})
                }
                crate::wire::VisionAction::ExtractFromCandidate { index } => {
                    serde_json::json!({"kind": "extractFromCandidate", "index": index})
                }
            };
            serde_json::json!({
                "confidence": r.confidence,
                "action": action,
            })
        });

        // Extract context
        let context = input.context.as_ref().map(|c| {
            serde_json::json!({
                "url": c.url,
                "candidates": c.candidates,
                "recentCommandKinds": c.recent_command_kinds,
            })
        });

        Self {
            image_b64: screenshot_png_b64,
            purpose: input.purpose.clone(),
            intent_kind: input.intent_kind.clone(),
            stuck: input.stuck.clone(),
            context,
            model_response,
            success,
            journey,
            step,
            error_message,
            timestamp: Some(chrono::Utc::now().to_rfc3339()),
            run_id,
            model_name,
            image_hash,
        }
    }
}

/// Thread-safe data collector that logs vision proposals to disk.
pub struct VisionDataCollector {
    config: DataCollectorConfig,
    buffer: Arc<Mutex<Vec<VisionTrainingExample>>>,
    last_flush: Mutex<Option<std::time::Instant>>,
}

impl VisionDataCollector {
    pub fn new(config: DataCollectorConfig) -> Self {
        // Create output directory if enabled
        if config.enabled {
            std::fs::create_dir_all(&config.output_dir).ok();
        }

        Self {
            config,
            buffer: Arc::new(Mutex::new(Vec::new())),
            last_flush: Mutex::new(None),
        }
    }

    pub fn config(&self) -> &DataCollectorConfig {
        &self.config
    }

    /// Log a vision proposal for training data collection.
    // Mirrors `VisionTrainingExample::new`'s flat record, one argument per
    // field.
    #[allow(clippy::too_many_arguments)]
    pub fn log_proposal(
        &self,
        screenshot_png_b64: String,
        input: &ProposeInput,
        response: Option<ProposeResponse>,
        journey: Option<String>,
        step: Option<String>,
        success: Option<bool>,
        error_message: Option<String>,
        run_id: Option<String>,
        model_name: Option<String>,
    ) {
        if !self.config.enabled {
            return;
        }

        let example = VisionTrainingExample::new(
            screenshot_png_b64,
            input,
            response,
            journey,
            step,
            success,
            error_message,
            run_id,
            model_name,
        );

        let mut buffer = self.buffer.lock().unwrap();
        buffer.push(example);
        let should_flush = buffer.len() >= 100
            || self.last_flush.lock().unwrap().is_none_or(|last| {
                last.elapsed().as_millis() as u64 >= self.config.flush_interval_ms
            });
        if should_flush {
            *self.last_flush.lock().unwrap() = Some(std::time::Instant::now());
            drop(buffer);
            self.flush();
        }
    }

    /// Flush buffered examples to disk.
    pub fn flush(&self) {
        let mut buffer = self.buffer.lock().unwrap();
        if buffer.is_empty() {
            return;
        }

        // Create output file if not exists
        let output_path = self.config.output_dir.join("training_data.jsonl");

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&output_path)
            .expect("failed to open training data file");

        let mut writer = std::io::BufWriter::new(file);

        for example in buffer.drain(..) {
            let json = serde_json::to_string(&example).unwrap();
            writer
                .write_all(json.as_bytes())
                .expect("failed to write training data");
            writer.write_all(b"\n").expect("failed to write newline");
        }

        writer.flush().expect("failed to flush training data");
    }

    /// Get collection statistics.
    pub fn stats(&self) -> (usize, usize) {
        let buffer = self.buffer.lock().unwrap();
        (
            buffer.len(),
            buffer.iter().filter(|e| e.success == Some(true)).count(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_example_creation() {
        let example = VisionTrainingExample::new(
            "dGVzdA==".to_string(), // base64 "test"
            &ProposeInput {
                purpose: "test".into(),
                intent_kind: "locate".into(),
                stuck: "targetMissing".into(),
                screenshot_png_b64: "dGVzdA==".to_string(),
                context: None,
            },
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );

        assert_eq!(example.purpose, "test");
        assert_eq!(example.intent_kind, "locate");
        assert!(example.image_hash.is_some());
    }

    #[test]
    fn candidate_actions_are_collected_as_index_only() {
        let input = ProposeInput {
            purpose: "select the target".into(),
            intent_kind: "fill".into(),
            stuck: "targetMissing".into(),
            screenshot_png_b64: "dGVzdA==".into(),
            context: None,
        };

        for action in [
            crate::wire::VisionAction::TypeIntoCandidate { index: 0 },
            crate::wire::VisionAction::ExtractFromCandidate { index: 1 },
        ] {
            let example = VisionTrainingExample::new(
                "dGVzdA==".into(),
                &input,
                Some(ProposeResponse {
                    confidence: 0.9,
                    action,
                }),
                None,
                None,
                Some(true),
                None,
                None,
                None,
            );
            let action = &example.model_response.expect("response")["action"];
            assert!(matches!(
                action["kind"].as_str(),
                Some("typeIntoCandidate" | "extractFromCandidate")
            ));
            assert!(action["index"].is_u64());
            assert!(action.get("text").is_none());
            assert!(action.get("value").is_none());
            assert!(action.get("clear_first").is_none());
        }
    }
}
