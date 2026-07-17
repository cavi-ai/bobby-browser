use types::{CommandClass, CommandError, ErrorCode, ErrorLayer, ExecutionReason, PrimitiveCommand};
use url::Url;

use crate::NetworkPolicy;

#[derive(Debug)]
pub enum EligibilityDecision {
    DirectHttp(ExecutionReason),
    Chromium(ExecutionReason),
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
                    return EligibilityDecision::Chromium(ExecutionReason::SemanticTargetRequired);
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
                    return EligibilityDecision::Denied(policy_error(
                        "download byte limit is outside the configured range",
                    ));
                }

                match validate_http_url(&download.url) {
                    Ok(()) => {
                        EligibilityDecision::DirectHttp(ExecutionReason::EligibleExplicitDownload)
                    }
                    Err(error) => EligibilityDecision::Denied(error),
                }
            }
            _ => {
                let _class: CommandClass = command.class();
                EligibilityDecision::Chromium(ExecutionReason::IneligibleCommand)
            }
        }
    }
}

fn validate_http_url(input: &str) -> Result<(), CommandError> {
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
