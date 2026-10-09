//! Browser-independent wait semantics. Adapters supply bounded observations;
//! this module owns matching, the continuous quiet window, and wait evidence.

use async_trait::async_trait;
use std::time::{Duration, Instant};
use types::{
    CommandError, ErrorCode, ErrorLayer, Evidence, TargetSpec, TextMatch, WaitCondition,
    WaitForCommand, WaitUntil,
};

/// A transport observation, before condition matching or evidence truncation.
pub enum WaitObservation {
    Pending,
    Element(bool),
    Text(Vec<String>),
    Url(String),
    Document(String),
    Network {
        in_flight: usize,
        excluded_classes: Vec<String>,
    },
}

#[async_trait]
pub trait WaitObserver: Send + Sync {
    async fn observe(&self, condition: &WaitCondition) -> Result<WaitObservation, CommandError>;
}

fn error(code: ErrorCode, message: impl Into<String>) -> CommandError {
    CommandError {
        code,
        message: message.into(),
        layer: ErrorLayer::Driver,
        retryable: false,
    }
}

pub fn bound_observed(value: &str) -> String {
    match value.char_indices().nth(types::MAX_WAIT_OBSERVED_CHARS) {
        Some((index, _)) => value[..index].to_owned(),
        None => value.to_owned(),
    }
}

pub fn text_matches(matcher: &TextMatch, value: &str) -> Result<bool, CommandError> {
    match matcher {
        TextMatch::Exact(expected) => Ok(value == expected),
        TextMatch::Contains(expected) => Ok(value.contains(expected)),
        TextMatch::Regex(pattern) => {
            if pattern.len() > 256 {
                return Err(error(
                    ErrorCode::InvalidRequest,
                    "wait regular expression exceeds configured limit",
                ));
            }
            regex::Regex::new(pattern)
                .map(|regex| regex.is_match(value))
                .map_err(|cause| error(ErrorCode::InvalidRequest, cause.to_string()))
        }
    }
}

pub fn is_page_scoped_text_target(target: &TargetSpec) -> bool {
    let field = |value: &Option<String>| value.as_deref().is_some_and(|v| !v.trim().is_empty());
    if field(&target.test_id)
        || field(&target.accessible_name)
        || field(&target.label)
        || target.text.is_some()
        || !target.attributes.is_empty()
        || !target.shadow_path.is_empty()
        || target.ordinal.is_some()
    {
        return false;
    }
    // A frame path scopes the document being read; it does not make body text
    // into an individual control. Both adapters must descend it before reading.
    let role = target
        .role
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let css = target
        .css
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let page_role = |role: &str| {
        [
            "RootWebArea",
            "document",
            "main",
            "body",
            "application",
            "generic",
        ]
        .iter()
        .any(|v| role.eq_ignore_ascii_case(v))
    };
    let page_css = |css: &str| {
        ["body", "html", ":root"]
            .iter()
            .any(|v| css.eq_ignore_ascii_case(v))
    };
    match (role, css) {
        (Some(role), None) => page_role(role),
        (None, Some(css)) => page_css(css),
        (Some(role), Some(css)) => page_role(role) && page_css(css),
        (None, None) => false,
    }
}

struct Match {
    satisfied: bool,
    observed: Option<String>,
    excluded_classes: Vec<String>,
}

fn match_observation(
    condition: &WaitCondition,
    observation: WaitObservation,
    window: &mut crate::policy::QuietWindow,
) -> Result<Match, CommandError> {
    let mut result = Match {
        satisfied: false,
        observed: None,
        excluded_classes: Vec::new(),
    };
    match (condition, observation) {
        (_, WaitObservation::Pending) => {}
        (WaitCondition::Element { .. }, WaitObservation::Element(satisfied)) => {
            result.satisfied = satisfied
        }
        (
            WaitCondition::Text { matcher, .. } | WaitCondition::Value { matcher, .. },
            WaitObservation::Text(values),
        ) => {
            for value in values {
                result.satisfied = text_matches(matcher, &value)?;
                result.observed = Some(value);
                if result.satisfied {
                    break;
                }
            }
        }
        (WaitCondition::Url { matcher }, WaitObservation::Url(value)) => {
            result.satisfied = text_matches(matcher, &value)?;
            result.observed = Some(value);
        }
        (WaitCondition::Document { ready }, WaitObservation::Document(value)) => {
            result.satisfied = match ready {
                WaitUntil::Commit => true,
                WaitUntil::DomContentLoaded | WaitUntil::Interactive => {
                    value == "interactive" || value == "complete"
                }
                WaitUntil::NetworkIdle => value == "complete",
            };
            result.observed = Some(value);
        }
        (
            WaitCondition::NetworkQuiet {
                idle_ms,
                max_in_flight,
                ..
            },
            WaitObservation::Network {
                in_flight,
                excluded_classes,
            },
        ) => {
            result.satisfied = window.observe(
                Instant::now(),
                in_flight,
                *max_in_flight,
                Duration::from_millis(*idle_ms),
            );
            result.excluded_classes = excluded_classes;
        }
        _ => {
            return Err(error(
                ErrorCode::BrowserCommandFailed,
                "wait adapter returned an observation for another condition",
            ))
        }
    }
    Ok(result)
}

/// Poll until success or the deadline. The final observation is performed at
/// the deadline, including page text; evidence counts every actual poll.
pub async fn poll_until(
    command: &WaitForCommand,
    observer: &impl WaitObserver,
) -> Result<Vec<Evidence>, CommandError> {
    if command.timeout_ms == 0 {
        return Err(error(
            ErrorCode::InvalidRequest,
            "wait timeout must be positive",
        ));
    }
    if let WaitCondition::Text { matcher, .. }
    | WaitCondition::Value { matcher, .. }
    | WaitCondition::Url { matcher } = &command.condition
    {
        text_matches(matcher, "")?;
    }
    let started = Instant::now();
    let deadline = started
        .checked_add(Duration::from_millis(command.timeout_ms))
        .ok_or_else(|| {
            error(
                ErrorCode::InvalidRequest,
                "wait timeout exceeds the clock range",
            )
        })?;
    let mut window = crate::policy::QuietWindow::default();
    let mut observations = 0;
    let timed_out = || {
        error(
            ErrorCode::WaitConditionTimedOut,
            format!(
                "wait condition was not satisfied within {}ms",
                command.timeout_ms
            ),
        )
    };
    loop {
        observations += 1;
        let observation = tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            observer.observe(&command.condition),
        )
        .await
        .map_err(|_| timed_out())??;
        let poll = match_observation(&command.condition, observation, &mut window)?;
        if poll.satisfied {
            return Ok(vec![Evidence::Wait {
                condition: command.condition.clone(),
                elapsed_ms: started.elapsed().as_millis() as u64,
                observations,
                excluded_classes: poll.excluded_classes,
                observed: poll.observed.map(|value| bound_observed(&value)),
            }]);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(error(
                ErrorCode::WaitConditionTimedOut,
                format!(
                    "wait condition was not satisfied within {}ms",
                    command.timeout_ms
                ),
            ));
        }
        tokio::time::sleep(remaining.min(Duration::from_millis(25))).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct SlowObserver;
    #[async_trait]
    impl WaitObserver for SlowObserver {
        async fn observe(&self, _: &WaitCondition) -> Result<WaitObservation, CommandError> {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(WaitObservation::Element(true))
        }
    }
    #[tokio::test]
    async fn slow_observation_cannot_succeed_after_the_wait_deadline() {
        let command = WaitForCommand {
            condition: WaitCondition::Element {
                target: Box::default(),
                state: types::ElementState::Attached,
            },
            timeout_ms: 10,
        };
        assert_eq!(
            poll_until(&command, &SlowObserver).await.unwrap_err().code,
            ErrorCode::WaitConditionTimedOut
        );
    }
    #[test]
    fn all_six_conditions_use_one_matching_contract() {
        let mut window = crate::policy::QuietWindow::default();
        let target = Box::default();
        let cases = [
            (
                WaitCondition::Element {
                    target,
                    state: types::ElementState::Visible,
                },
                WaitObservation::Element(true),
            ),
            (
                WaitCondition::Text {
                    target: Box::default(),
                    matcher: TextMatch::Contains("ready".into()),
                },
                WaitObservation::Text(vec!["wrong".into(), "ready now".into()]),
            ),
            (
                WaitCondition::Value {
                    target: Box::default(),
                    matcher: TextMatch::Exact("漢字".into()),
                },
                WaitObservation::Text(vec!["漢字".into()]),
            ),
            (
                WaitCondition::Url {
                    matcher: TextMatch::Regex("/done$".into()),
                },
                WaitObservation::Url("https://example.test/done".into()),
            ),
            (
                WaitCondition::Document {
                    ready: WaitUntil::Interactive,
                },
                WaitObservation::Document("complete".into()),
            ),
            (
                WaitCondition::NetworkQuiet {
                    idle_ms: 0,
                    max_in_flight: 0,
                    ignore_url_substrings: vec![],
                    ignore_resource_types: vec![],
                    ignore_long_lived: false,
                },
                WaitObservation::Network {
                    in_flight: 0,
                    excluded_classes: vec!["websocket".into()],
                },
            ),
        ];
        for (condition, observation) in cases {
            assert!(
                match_observation(&condition, observation, &mut window)
                    .unwrap()
                    .satisfied
            );
        }
        assert_eq!(
            text_matches(&TextMatch::Regex("[".into()), "")
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            text_matches(&TextMatch::Regex("x".repeat(257)), "")
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            bound_observed(&"漢".repeat(types::MAX_WAIT_OBSERVED_CHARS + 1))
                .chars()
                .count(),
            types::MAX_WAIT_OBSERVED_CHARS
        );
    }
}
