use scraper::Selector;
use types::{CommandClass, CommandError, ErrorCode, ErrorLayer, ExecutionReason, PrimitiveCommand};
use url::Url;

use crate::NetworkPolicy;

#[derive(Debug)]
pub enum EligibilityDecision {
    DirectHttp(ExecutionReason),
    /// Needs a live browser, whichever engine the session holds.
    Browser(ExecutionReason),
    Denied(CommandError),
}

#[derive(Debug, Clone)]
pub struct EligibilityPolicy {
    network: NetworkPolicy,
}

impl EligibilityPolicy {
    pub fn new(network: NetworkPolicy) -> Self {
        Self { network }
    }

    pub fn classify(&self, command: &PrimitiveCommand, page_url: &str) -> EligibilityDecision {
        match command {
            PrimitiveCommand::Inspect(inspect) => {
                if inspect.target.is_some() {
                    return EligibilityDecision::Browser(ExecutionReason::SemanticTargetRequired);
                }
                if inspect
                    .selector
                    .as_deref()
                    .is_some_and(|selector| Selector::parse(selector).is_err())
                {
                    return EligibilityDecision::Browser(ExecutionReason::IneligibleCommand);
                }

                match validate_http_url(page_url) {
                    Ok(()) => {
                        EligibilityDecision::DirectHttp(ExecutionReason::EligibleStaticDocument)
                    }
                    Err(error) => EligibilityDecision::Denied(error),
                }
            }
            PrimitiveCommand::DownloadUrl(download) => {
                if download.max_bytes == 0
                    || download.max_bytes > self.network.max_download_bytes as u64
                {
                    return EligibilityDecision::Denied(download_limit_error(
                        self.network.max_download_bytes,
                    ));
                }

                match validate_http_url(&download.url) {
                    Ok(()) => {
                        EligibilityDecision::DirectHttp(ExecutionReason::EligibleExplicitDownload)
                    }
                    Err(error) => EligibilityDecision::Denied(error),
                }
            }
            // The catch-all below already routes `EvaluateJavaScript` (and every other
            // command class) to the browser — JavaScript evaluation always requires a
            // live browser context and can never be satisfied by the direct-HTTP path.
            _ => {
                let _class: CommandClass = command.class();
                EligibilityDecision::Browser(ExecutionReason::IneligibleCommand)
            }
        }
    }
}

pub(crate) fn validate_http_url(input: &str) -> Result<(), CommandError> {
    let url = Url::parse(input).map_err(|_| policy_error("URL is invalid"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(policy_error("URL scheme is not permitted"));
    }
    if url.host().is_none() {
        return Err(policy_error("URL must include a host"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(policy_error("credentials in URLs are not permitted"));
    }
    Ok(())
}

pub(crate) fn policy_error(message: impl Into<String>) -> CommandError {
    CommandError {
        code: ErrorCode::NetworkPolicyDenied,
        message: message.into(),
        layer: ErrorLayer::Network,
        retryable: false,
    }
}

pub(crate) fn download_limit_error(max_download_bytes: usize) -> CommandError {
    CommandError {
        code: ErrorCode::InvalidRequest,
        message: format!("maxBytes must be between 1 and {max_download_bytes} for this runtime"),
        layer: ErrorLayer::Network,
        retryable: false,
    }
}
