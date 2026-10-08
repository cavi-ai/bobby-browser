use serde::de::DeserializeOwned;

use crate::upstream::UpstreamError;

// Allows the 64 KiB extract value, JSON escaping, and provider envelope metadata.
// Count decoded bytes: reqwest removes Content-Length when decompressing a body.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

pub(crate) async fn read_json<T: DeserializeOwned>(
    mut response: reqwest::Response,
    provider: &str,
) -> Result<T, UpstreamError> {
    let status = response.status();
    if !status.is_success() {
        let code = status.as_u16();
        let class = if matches!(code, 401 | 403) {
            "authentication failed"
        } else {
            "request rejected"
        };
        // Drop the response without buffering untrusted error text or exposing it.
        return Err(UpstreamError::Rejected(format!(
            "{provider} upstream {class}; status={code}"
        )));
    }

    let oversized = || {
        UpstreamError::Invalid(format!(
            "{provider} response exceeded the {MAX_RESPONSE_BYTES} byte limit"
        ))
    };
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(oversized());
    }

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| UpstreamError::Invalid(format!("{provider} response read failed")))?
    {
        if chunk.len() > MAX_RESPONSE_BYTES - body.len() {
            return Err(oversized());
        }
        let length = body.len() + chunk.len();
        if length > body.capacity() {
            let capacity = (body.capacity().max(4096) * 2)
                .max(length)
                .min(MAX_RESPONSE_BYTES);
            body.reserve_exact(capacity - body.len());
        }
        body.extend_from_slice(&chunk);
    }

    // Decoder diagnostics can contain upstream values; keep them out of errors.
    serde_json::from_slice(&body)
        .map_err(|_| UpstreamError::Invalid(format!("{provider} response parse failed")))
}
