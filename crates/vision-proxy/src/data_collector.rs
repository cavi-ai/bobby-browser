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
    pub privacy_version: u8,
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
    #[serde(skip_serializing)]
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
        input: &ProposeInput,
        response: Option<ProposeResponse>,
        journey: Option<String>,
        step: Option<String>,
        success: Option<bool>,
        error_message: Option<String>,
        run_id: Option<String>,
        model_name: Option<String>,
    ) -> Option<Self> {
        let screenshot_png_b64 = input.corpus_screenshot_png_b64.clone()?;
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
            let action = serde_json::to_value(&r.action).unwrap_or_default();
            let action = intent_engine::sanitize_corpus_action(&action, None);
            serde_json::json!({
                "confidence": r.confidence,
                "action": action,
            })
        });

        // Extract context
        let context = input.context.as_ref().map(|c| {
            serde_json::json!({
                "url": c.url.as_deref().and_then(intent_engine::sanitize_corpus_url),
                "candidates": c.candidates.iter().map(|candidate| serde_json::json!({
                    "role": intent_engine::sanitize_corpus_label(&candidate.role),
                    "name": intent_engine::sanitize_corpus_label(&candidate.name),
                    "ordinal": candidate.ordinal,
                })).collect::<Vec<_>>(),
                "recentCommandKinds": c.recent_command_kinds.iter()
                    .map(|kind| intent_engine::sanitize_corpus_label(kind))
                    .collect::<Vec<_>>(),
            })
        });

        Some(Self {
            privacy_version: 1,
            image_b64: screenshot_png_b64,
            purpose: intent_engine::sanitize_corpus_label(&input.purpose),
            intent_kind: intent_engine::sanitize_corpus_label(&input.intent_kind),
            stuck: intent_engine::sanitize_corpus_label(&input.stuck),
            context,
            model_response,
            success,
            journey: journey.map(|value| intent_engine::sanitize_corpus_label(&value)),
            step: step.map(|value| intent_engine::sanitize_corpus_label(&value)),
            error_message: error_message.map(|_| "[redacted]".into()),
            timestamp: Some(chrono::Utc::now().to_rfc3339()),
            run_id: run_id.map(|value| intent_engine::sanitize_corpus_label(&value)),
            model_name: model_name.map(|value| intent_engine::sanitize_corpus_label(&value)),
            image_hash,
        })
    }
}

/// Thread-safe data collector that logs vision proposals to disk.
pub struct VisionDataCollector {
    config: DataCollectorConfig,
    storage_ready: bool,
    buffer: Arc<Mutex<Vec<VisionTrainingExample>>>,
    last_flush: Mutex<Option<std::time::Instant>>,
}

impl VisionDataCollector {
    pub fn new(config: DataCollectorConfig) -> Self {
        let storage_ready = !config.enabled
            || std::fs::create_dir_all(&config.output_dir)
                .and_then(|_| set_private_dir_permissions(&config.output_dir))
                .is_ok();

        Self {
            config,
            storage_ready,
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
        input: &ProposeInput,
        response: Option<ProposeResponse>,
        journey: Option<String>,
        step: Option<String>,
        success: Option<bool>,
        error_message: Option<String>,
        run_id: Option<String>,
        model_name: Option<String>,
    ) {
        if !self.config.enabled || !self.storage_ready {
            return;
        }

        let Some(example) = VisionTrainingExample::new(
            input,
            response,
            journey,
            step,
            success,
            error_message,
            run_id,
            model_name,
        ) else {
            return;
        };

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

        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options
            .open(&output_path)
            .expect("failed to open training data file");
        if set_private_file_permissions(&output_path).is_err() {
            return;
        }

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

#[cfg(unix)]
fn set_private_dir_permissions(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_example_creation() {
        let example = VisionTrainingExample::new(
            &ProposeInput {
                purpose: "Enter maya@atlas.example in the email field".into(),
                intent_kind: "locate".into(),
                stuck: "targetMissing".into(),
                screenshot_png_b64: "dGVzdA==".to_string(),
                corpus_screenshot_png_b64: Some("dGVzdA==".to_string()),
                context: None,
            },
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("sanitized screenshot");

        assert_eq!(example.purpose, "Enter [redacted] in the email field");
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
            corpus_screenshot_png_b64: Some("dGVzdA==".into()),
            context: None,
        };

        for action in [
            crate::wire::VisionAction::TypeIntoCandidate { index: 0 },
            crate::wire::VisionAction::ExtractFromCandidate { index: 1 },
        ] {
            let example = VisionTrainingExample::new(
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
            )
            .expect("sanitized screenshot");
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

    #[test]
    fn legacy_payload_actions_are_not_persisted() {
        let input = ProposeInput {
            purpose: "fill password".into(),
            intent_kind: "fill".into(),
            stuck: "targetMissing".into(),
            screenshot_png_b64: "cmF3".into(),
            corpus_screenshot_png_b64: Some("c2FuaXRpemVk".into()),
            context: None,
        };
        let example = VisionTrainingExample::new(
            &input,
            Some(ProposeResponse {
                confidence: 0.9,
                action: crate::wire::VisionAction::TypeText {
                    text: "must-not-survive".into(),
                },
            }),
            None,
            None,
            Some(true),
            Some("Authorization: Bearer must-not-survive".into()),
            None,
            None,
        )
        .expect("sanitized screenshot");
        let encoded = serde_json::to_string(&example).unwrap();
        assert_eq!(example.model_response.unwrap()["action"]["kind"], "abstain");
        assert!(!encoded.contains("must-not-survive"));
        assert!(!encoded.contains("error_message"));
    }

    #[test]
    fn raw_only_input_is_not_collectable() {
        let input = ProposeInput {
            purpose: "fill password".into(),
            intent_kind: "fill".into(),
            stuck: "targetMissing".into(),
            screenshot_png_b64: "cmF3".into(),
            corpus_screenshot_png_b64: None,
            context: None,
        };
        assert!(
            VisionTrainingExample::new(&input, None, None, None, None, None, None, None,).is_none()
        );
    }

    #[test]
    fn collection_stays_disabled_when_private_storage_cannot_be_created() {
        let unique = format!(
            "bobby-vision-collector-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let output_dir = std::env::temp_dir().join(unique);
        std::fs::write(&output_dir, b"not a directory").unwrap();
        let collector = VisionDataCollector::new(DataCollectorConfig {
            output_dir: output_dir.clone(),
            enabled: true,
            flush_interval_ms: 0,
        });
        let input = ProposeInput {
            purpose: "select the target".into(),
            intent_kind: "locate".into(),
            stuck: "targetMissing".into(),
            screenshot_png_b64: "cmF3".into(),
            corpus_screenshot_png_b64: Some("c2FuaXRpemVk".into()),
            context: None,
        };

        collector.log_proposal(&input, None, None, None, None, None, None, None);

        assert_eq!(collector.stats(), (0, 0));
        assert_eq!(std::fs::read(&output_dir).unwrap(), b"not a directory");
        std::fs::remove_file(output_dir).unwrap();
    }
}
