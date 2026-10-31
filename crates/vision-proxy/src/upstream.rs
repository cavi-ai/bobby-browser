use async_trait::async_trait;

use crate::wire::{ExtractResponse, ProposeResponse};

#[async_trait]
pub trait Upstream: Send + Sync {
    async fn propose(&self, input: ProposeInput) -> Result<ProposeResponse, UpstreamError>;
    async fn extract(&self, input: ExtractInput) -> Result<ExtractResponse, UpstreamError>;
}

pub struct ProposeInput {
    pub purpose: String,
    pub intent_kind: String,
    pub stuck: String,
    pub screenshot_png_b64: Option<String>,
    pub corpus_screenshot_png_b64: Option<String>,
    pub context: Option<crate::wire::ProposeContext>,
}

impl ProposeInput {
    /// The screenshot, for upstreams that read pixels.
    pub(crate) fn screenshot(&self) -> Result<&str, UpstreamError> {
        self.screenshot_png_b64
            .as_deref()
            .ok_or_else(|| UpstreamError::Rejected("this upstream needs a screenshot".into()))
    }

    /// The per-request prompt text: task, page, candidates, recent commands.
    pub(crate) fn prompt_text(&self) -> String {
        let mut text = format!(
            "purpose: {}\nintentKind: {}\nstuck: {}",
            self.purpose, self.intent_kind, self.stuck
        );
        let context = self.context.as_ref();
        if let Some(url) = context.and_then(|context| context.url.as_ref()) {
            text.push_str(&format!("\nurl: {url}"));
        }
        let candidates = context.map_or(&[][..], |context| &context.candidates[..]);
        if !candidates.is_empty() {
            text.push_str("\ncandidates:");
            for candidate in candidates {
                text.push_str(&format!(
                    "\n- {} \"{}\"{}",
                    candidate.role,
                    candidate.name,
                    candidate
                        .ordinal
                        .map(|ordinal| format!(" (#{ordinal})"))
                        .unwrap_or_default()
                ));
            }
        } else if let Some((candidate_kind, legacy_kind)) =
            types::candidate_action_kinds_for_intent(&self.intent_kind)
        {
            // Without a list, a candidate index points at nothing.
            text.push_str(&format!(
                "\ncandidates: none; answer with {legacy_kind}, not {candidate_kind}"
            ));
        }
        if let Some(context) = context.filter(|context| !context.recent_command_kinds.is_empty()) {
            text.push_str(&format!(
                "\nrecentCommands: {}",
                context.recent_command_kinds.join(", ")
            ));
        }
        text
    }
}

pub struct ExtractInput {
    pub schema: serde_json::Value,
    pub content: String,
    pub purpose: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    #[error("upstream transport failed: {0}")]
    Transport(String),
    #[error("upstream rejected the request: {0}")]
    Rejected(String),
    #[error("upstream returned an invalid payload: {0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{ProposeContext, ProposeContextCandidate};

    fn input(intent_kind: &str, candidates: Vec<ProposeContextCandidate>) -> ProposeInput {
        ProposeInput {
            purpose: "p".into(),
            intent_kind: intent_kind.into(),
            stuck: "targetMissing".into(),
            screenshot_png_b64: None,
            corpus_screenshot_png_b64: None,
            context: Some(ProposeContext {
                url: None,
                candidates,
                recent_command_kinds: vec![],
            }),
        }
    }

    #[test]
    fn a_request_without_candidates_names_the_action_to_answer_with() {
        for (intent, line) in [
            (
                "locate",
                "\ncandidates: none; answer with click, not clickCandidate",
            ),
            (
                "fill",
                "\ncandidates: none; answer with typeText, not typeIntoCandidate",
            ),
            (
                "extract",
                "\ncandidates: none; answer with extractValue, not extractFromCandidate",
            ),
        ] {
            assert!(
                input(intent, vec![]).prompt_text().contains(line),
                "{intent}"
            );
        }
        assert!(!input("detectChallenge", vec![])
            .prompt_text()
            .contains("candidates: none"));
        let listed = input(
            "locate",
            vec![ProposeContextCandidate {
                role: "button".into(),
                name: "Save".into(),
                ordinal: None,
            }],
        )
        .prompt_text();
        assert!(
            listed.contains("\ncandidates:\n- button \"Save\""),
            "{listed}"
        );
        assert!(!listed.contains("candidates: none"), "{listed}");
    }
}
