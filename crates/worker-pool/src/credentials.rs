//! Credentials an agent fills by name. The operator's configured command
//! prints the secret; it runs only for a field on the credential's origin,
//! and the secret is filled, never returned, logged or journaled.

use std::{process::Stdio, time::Duration};

use config::CredentialConfig;
use tokio::io::AsyncReadExt;
use types::{CommandError, ErrorCode, ErrorLayer};
use zeroize::Zeroize;
pub use zeroize::Zeroizing;

/// Long enough for a vault command that waits on the operator's unlock.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_SECRET_BYTES: usize = 16 * 1024;

/// A resolved secret: zeroed on drop and never formatted.
pub struct Secret(String);

impl Secret {
    #[cfg(test)]
    fn expose(&self) -> &str {
        &self.0
    }

    /// Function body, with `el` bound to the field, that sets a password
    /// field to the secret through the native setter, so a framework's own
    /// value tracking sees the change, then fires `input` and `change` like a
    /// user's edit. The field is never focused. Returns whether it was set.
    pub fn fill_script(&self) -> Zeroizing<String> {
        let literal = Zeroizing::new(serde_json::to_string(&self.0).unwrap_or_default());
        Zeroizing::new(format!(
            "if (!(el instanceof HTMLInputElement) || el.type !== 'password') return false; \
             Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(el, {}); \
             el.dispatchEvent(new Event('input', {{ bubbles: true }})); \
             el.dispatchEvent(new Event('change', {{ bubbles: true }})); \
             return true;",
            literal.as_str()
        ))
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

fn error(code: ErrorCode, message: String) -> CommandError {
    CommandError {
        code,
        message,
        layer: ErrorLayer::Driver,
        retryable: false,
    }
}

/// `origin` as a browser serializes it, or `None` for an opaque origin.
pub fn serialized_origin(url_or_origin: &str) -> Option<String> {
    let origin = url::Url::parse(url_or_origin).ok()?.origin();
    origin.is_tuple().then(|| origin.ascii_serialization())
}

/// The credential `name` configured for a field on `origin`. Nothing runs
/// unless a credential of that name is configured for exactly that origin.
pub fn select<'a>(
    credentials: &'a [CredentialConfig],
    name: &str,
    origin: &str,
) -> Result<&'a CredentialConfig, CommandError> {
    let named = || {
        credentials
            .iter()
            .filter(|credential| credential.name == name)
    };
    if let Some(credential) =
        named().find(|credential| serialized_origin(&credential.origin).as_deref() == Some(origin))
    {
        return Ok(credential);
    }
    if named().next().is_some() {
        return Err(error(
            ErrorCode::PolicyDenied,
            format!("credential `{name}` is not configured for {origin}"),
        ));
    }
    let here: Vec<&str> = credentials
        .iter()
        .filter(|credential| serialized_origin(&credential.origin).as_deref() == Some(origin))
        .map(|credential| credential.name.as_str())
        .collect();
    Err(error(
        ErrorCode::InvalidRequest,
        if here.is_empty() {
            format!(
                "no credential named `{name}` is configured, and none is configured for {origin}"
            )
        } else {
            format!(
                "no credential named `{name}` is configured; {origin} has {}",
                here.join(", ")
            )
        },
    ))
}

/// Runs the credential's command and reads the secret it prints. Its stderr
/// and output never reach an error.
pub async fn resolve(credential: &CredentialConfig) -> Result<Secret, CommandError> {
    let failed = |detail: &str| {
        error(
            ErrorCode::Internal,
            format!("credential `{}` command {detail}", credential.name),
        )
    };
    let (program, args) = credential
        .command
        .split_first()
        .ok_or_else(|| failed("is empty"))?;
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| failed("could not start"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| failed("could not start"))?;
    // Sized up front so the buffer never reallocates and leaves a copy behind.
    let mut output = Zeroizing::new(Vec::with_capacity(MAX_SECRET_BYTES + 3));
    let status = tokio::time::timeout(COMMAND_TIMEOUT, async {
        (&mut stdout)
            .take(MAX_SECRET_BYTES as u64 + 3)
            .read_to_end(&mut output)
            .await?;
        drop(stdout);
        child.wait().await
    })
    .await
    .map_err(|_| failed("timed out"))?
    .map_err(|_| failed("could not be read"))?;
    if output.len() > MAX_SECRET_BYTES + 2 {
        return Err(failed("printed more than 16 KiB"));
    }
    if !status.success() {
        return Err(failed(&format!("exited with {status}")));
    }
    secret_from(output).map_err(failed)
}

/// The printed secret without its one trailing line break.
fn secret_from(mut output: Zeroizing<Vec<u8>>) -> Result<Secret, &'static str> {
    if output.last() == Some(&b'\n') {
        output.pop();
        if output.last() == Some(&b'\r') {
            output.pop();
        }
    }
    let text = std::str::from_utf8(&output).map_err(|_| "printed text that is not UTF-8")?;
    if text.is_empty() {
        return Err("printed nothing");
    }
    if text.len() > MAX_SECRET_BYTES {
        return Err("printed more than 16 KiB");
    }
    if text.chars().any(char::is_control) {
        return Err("printed more than one line or control characters");
    }
    Ok(Secret(text.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "fixture-Pass-42";

    fn credential(origin: &str, script: &str) -> CredentialConfig {
        CredentialConfig {
            name: "work".into(),
            origin: origin.into(),
            command: vec!["/bin/sh".into(), "-c".into(), script.into()],
        }
    }

    #[test]
    fn only_the_named_credential_for_the_exact_origin_is_selected() {
        let credentials = [
            credential("https://login.example.test/", "true"),
            CredentialConfig {
                name: "other".into(),
                ..credential("https://other.example.test", "true")
            },
        ];
        let chosen = select(&credentials, "work", "https://login.example.test").unwrap();
        assert_eq!(chosen.origin, "https://login.example.test/");

        let elsewhere = select(&credentials, "work", "https://other.example.test").unwrap_err();
        assert_eq!(elsewhere.code, ErrorCode::PolicyDenied);

        let unknown = select(&credentials, "missing", "https://other.example.test").unwrap_err();
        assert_eq!(unknown.code, ErrorCode::InvalidRequest);
        assert!(unknown.message.contains("has other"), "{}", unknown.message);

        let opaque = select(&credentials, "work", "null").unwrap_err();
        assert_eq!(opaque.code, ErrorCode::PolicyDenied);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_command_prints_the_secret_and_its_failures_stay_private() {
        let printed = credential(
            "https://login.example.test",
            &format!("printf '%s\\n' '{SECRET}'; printf '%s' '{SECRET}' >&2"),
        );
        let secret = resolve(&printed).await.unwrap();
        assert_eq!(secret.expose(), SECRET);
        assert!(!format!("{secret:?}").contains(SECRET));

        for (script, detail) in [
            (format!("printf '%s' '{SECRET}'; exit 3"), "exited with"),
            (
                format!("printf '%s\\n%s\\n' '{SECRET}' '{SECRET}'"),
                "more than one line",
            ),
            ("printf ''".to_owned(), "printed nothing"),
            (
                "head -c 20000 /dev/zero | tr '\\0' a".to_owned(),
                "more than 16 KiB",
            ),
        ] {
            let error = resolve(&credential("https://login.example.test", &script))
                .await
                .unwrap_err();
            assert!(
                error.message.contains(detail),
                "{script}: {}",
                error.message
            );
            assert!(
                !format!("{error:?}").contains(SECRET),
                "{script}: {error:?}"
            );
        }
    }
}
